use crate::agents::extension::PlatformExtensionContext;
use crate::agents::extension_manager::get_tool_owner;
use crate::agents::mcp_client::{Error, McpClientTrait};
use anyhow::Result;
use async_trait::async_trait;
use indoc::indoc;
use pctx_code_mode::model::{CallbackConfig, ExecuteInput, GetFunctionDetailsInput};
use pctx_code_mode::{CallbackRegistry, CodeMode};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, InitializeResult, JsonObject,
    ListToolsResult, RawContent, Role, ServerCapabilities, Tool as McpTool, ToolAnnotations,
};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Thread-safe store for non-text content captured during code execution.
/// Maps token strings (e.g. "cref_1") to the original rich Content items.
type ContentStore = Arc<std::sync::Mutex<HashMap<String, Vec<Content>>>>;

/// Global counter for generating unique content reference tokens.
static CONTENT_REF_COUNTER: AtomicU64 = AtomicU64::new(0);

pub static EXTENSION_NAME: &str = "code_execution";

pub struct CodeExecutionClient {
    info: InitializeResult,
    context: PlatformExtensionContext,
    state: RwLock<Option<CodeModeState>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct ToolGraphNode {
    /// Tool name in format "server/tool" (e.g., "developer/shell")
    tool: String,
    /// Brief description of what this call does (e.g., "list files in /src")
    description: String,
    /// Indices of nodes this depends on (empty if no dependencies)
    #[serde(default)]
    depends_on: Vec<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecuteWithToolGraph {
    #[serde(flatten)]
    input: ExecuteInput,
    /// DAG of tool calls showing execution flow. Each node represents a tool call.
    /// Use depends_on to show data flow (e.g., node 1 uses output from node 0).
    #[serde(default)]
    tool_graph: Vec<ToolGraphNode>,
}

impl CodeExecutionClient {
    pub fn new(context: PlatformExtensionContext) -> Result<Self> {
        let info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(EXTENSION_NAME.to_string(), "1.0.0".to_string())
                    .with_title("Code Mode"),
            )
            .with_instructions(indoc! {r#"
                BATCH MULTIPLE TOOL CALLS INTO ONE execute CALL.

                This extension exists to reduce round-trips. When a task requires multiple tool calls:
                - WRONG: Multiple execute calls, each with one tool
                - RIGHT: One execute call with a script that calls all needed tools

                IMPORTANT: All tool calls are ASYNC. Use await for each call.

                Workflow:
                    1. Use the list_functions and get_function_details tools to discover tools and signatures
                    2. Write ONE script that calls ALL tools needed for the task, no need to import anything,
                       all the namespaces returned by list_functions and get_function_details will be available
                    3. Chain results: use output from one tool as input to the next
                    4. Only return the data you need — tools can have very large responses.
            "#}.to_string());

        Ok(Self {
            info,
            context,
            state: RwLock::new(None),
        })
    }

    async fn load_callback_configs(&self, session_id: &str) -> Option<Vec<CallbackConfig>> {
        let manager = self
            .context
            .extension_manager
            .as_ref()
            .and_then(|w| w.upgrade())?;

        let tools = manager
            .get_prefixed_tools_excluding(session_id, EXTENSION_NAME)
            .await
            .ok()?;
        let mut cfgs = vec![];
        for tool in tools {
            let full_name = tool.name.to_string();
            let (namespace, name) = if let Some((server, tool_name)) = full_name.split_once("__") {
                (server.to_string(), tool_name.to_string())
            } else if let Some(owner) = get_tool_owner(&tool) {
                (owner, full_name)
            } else {
                continue;
            };
            cfgs.push(CallbackConfig {
                name,
                namespace,
                description: tool.description.as_ref().map(|d| d.to_string()),
                input_schema: Some(json!(tool.input_schema)),
                output_schema: tool.output_schema.as_ref().map(|s| json!(s)),
            })
        }
        Some(cfgs)
    }

    /// Get the cached CodeMode, rebuilding if callback configs have changed
    async fn get_code_mode(&self, session_id: &str) -> Result<CodeMode, String> {
        let cfgs = self
            .load_callback_configs(session_id)
            .await
            .ok_or("Failed to load callback configs")?;
        let current_hash = CodeModeState::hash(&cfgs);

        // Use cache if no state change
        {
            let guard = self.state.read().await;
            if let Some(state) = guard.as_ref() {
                if state.hash == current_hash {
                    return Ok(state.code_mode.clone());
                }
            }
        }

        // Rebuild CodeMode & cache
        let mut guard = self.state.write().await;
        // Double-check after acquiring write lock
        if let Some(state) = guard.as_ref() {
            if state.hash == current_hash {
                return Ok(state.code_mode.clone());
            }
        }

        let state = CodeModeState::new(cfgs)?;
        let code_mode = state.code_mode.clone();
        *guard = Some(state);
        Ok(code_mode)
    }

    /// Build a CallbackRegistry with all tool callbacks registered.
    /// Returns the registry and a ContentStore that will accumulate non-text content
    /// produced by tool callbacks during execution.
    fn build_callback_registry(
        &self,
        session_id: &str,
        code_mode: &CodeMode,
    ) -> Result<(CallbackRegistry, ContentStore), String> {
        let manager = self
            .context
            .extension_manager
            .as_ref()
            .and_then(|w| w.upgrade())
            .ok_or("Extension manager not available")?;

        let content_store: ContentStore = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let registry = CallbackRegistry::default();
        for cfg in code_mode.callbacks() {
            let full_name = format!("{}__{}", &cfg.namespace, &cfg.name);
            let callback = create_tool_callback(
                session_id.to_string(),
                full_name,
                manager.clone(),
                content_store.clone(),
            );
            registry
                .add(&cfg.id(), callback)
                .map_err(|e| format!("Failed to register callback: {e}"))?;
        }

        Ok((registry, content_store))
    }

    /// Handle the list_functions tool call
    async fn handle_list_functions(&self, session_id: &str) -> Result<Vec<Content>, String> {
        let code_mode = self.get_code_mode(session_id).await?;
        let output = code_mode.list_functions();

        Ok(vec![Content::text(output.code)])
    }

    /// Handle the get_function_details tool call
    async fn handle_get_function_details(
        &self,
        session_id: &str,
        arguments: Option<JsonObject>,
    ) -> Result<Vec<Content>, String> {
        let input: GetFunctionDetailsInput = arguments
            .map(|args| serde_json::from_value(Value::Object(args)))
            .transpose()
            .map_err(|e| format!("Failed to parse arguments: {e}"))?
            .ok_or("Missing arguments for get_function_details")?;

        let code_mode = self.get_code_mode(session_id).await?;
        let output = code_mode.get_function_details(input);

        Ok(vec![Content::text(output.code)])
    }

    /// Handle the execute tool call
    async fn handle_execute(
        &self,
        session_id: &str,
        arguments: Option<JsonObject>,
    ) -> Result<Vec<Content>, String> {
        let args: ExecuteWithToolGraph = arguments
            .map(|args| serde_json::from_value(Value::Object(args)))
            .transpose()
            .map_err(|e| format!("Failed to parse arguments: {e}"))?
            .ok_or("Missing arguments for execute")?;

        let code_mode = self.get_code_mode(session_id).await?;
        let (registry, content_store) = self.build_callback_registry(session_id, &code_mode)?;
        let code = args.input.code.clone();

        // Deno runtime is not Send, so we need to run it in a blocking task
        // with its own tokio runtime
        let output = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("Failed to create runtime: {e}"))?;

            rt.block_on(async move {
                code_mode
                    .execute(&code, Some(registry))
                    .await
                    .map_err(|e| format!("Execution error: {e}"))
            })
        })
        .await
        .map_err(|e| format!("Execution task failed: {e}"))??;

        // Collect any rich content referenced by tokens in the output
        let mut rich_contents = Vec::new();
        if let Some(ref val) = output.output {
            let store = content_store.lock().unwrap();
            collect_rich_content(val, &store, &mut rich_contents);
        }

        // If the entire return value was just a content ref, return only
        // the resolved rich content. Otherwise return the text output
        // (with refs intact) plus the rich content alongside it.
        // Only treat as a pure content ref if the object is exactly the shape
        // we created (just _goose_content_ref + text_result, nothing extra).
        let is_pure_ref = output
            .output
            .as_ref()
            .and_then(|v| v.as_object())
            .is_some_and(|m| {
                m.contains_key("_goose_content_ref")
                    && m.len() <= 2
                    && m.keys()
                        .all(|k| k == "_goose_content_ref" || k == "text_result")
            });

        if is_pure_ref && !rich_contents.is_empty() {
            // Always include a text fallback so providers that only serialize
            // text content (OpenAI, Codex, Anthropic) still produce a tool result
            // for the model. The rich content carries audience metadata and will
            // be filtered by downstream audience logic.
            let mut contents = vec![Content::text("Tool returned rich content.")];
            contents.extend(rich_contents);
            Ok(contents)
        } else {
            let return_val = serde_json::to_string_pretty(&output.output)
                .unwrap_or_else(|_| json!(&output.output).to_string());
            let mut contents = vec![Content::text(format_output(
                output.success,
                &return_val,
                &output.stderr,
            ))];
            contents.extend(rich_contents);
            Ok(contents)
        }
    }
}

fn create_tool_callback(
    session_id: String,
    full_name: String,
    manager: Arc<crate::agents::ExtensionManager>,
    content_store: ContentStore,
) -> pctx_code_mode::CallbackFn {
    Arc::new(move |args: Option<Value>| {
        let session_id = session_id.clone();
        let full_name = full_name.clone();
        let manager = manager.clone();
        let content_store = content_store.clone();
        Box::pin(async move {
            let tool_call = {
                let mut params = CallToolRequestParams::new(full_name);
                if let Some(args) = args.and_then(|v| v.as_object().cloned()) {
                    params = params.with_arguments(args);
                }
                params
            };
            match manager
                .dispatch_tool_call(&session_id, tool_call, None, CancellationToken::new())
                .await
            {
                Ok(dispatch_result) => match dispatch_result.result.await {
                    Ok(result) => {
                        if let Some(sc) = &result.structured_content {
                            Ok(serde_json::to_value(sc).unwrap_or(Value::Null))
                        } else {
                            // Separate text content from non-text (resources, images, etc.)
                            let mut text_parts = Vec::new();
                            let mut rich_contents = Vec::new();

                            for content in &result.content {
                                match &content.raw {
                                    RawContent::Text(t) => {
                                        // Only include text meant for assistant
                                        if content.audience().is_none_or(|audiences| {
                                            audiences.is_empty()
                                                || audiences.contains(&Role::Assistant)
                                        }) {
                                            text_parts.push(t.text.clone());
                                        }
                                    }
                                    _ => {
                                        // Non-text content (resources, images, blobs) — store for later
                                        rich_contents.push(content.clone());
                                    }
                                }
                            }

                            let text = text_parts.join("\n");
                            let text_value: Value =
                                serde_json::from_str(&text).unwrap_or(Value::String(text));

                            // If there's non-text content, store it and return a token reference
                            if !rich_contents.is_empty() {
                                let token = format!(
                                    "cref_{}",
                                    CONTENT_REF_COUNTER.fetch_add(1, Ordering::Relaxed)
                                );
                                content_store
                                    .lock()
                                    .unwrap()
                                    .insert(token.clone(), rich_contents);
                                Ok(json!({
                                    "_goose_content_ref": token,
                                    "text_result": text_value,
                                }))
                            } else {
                                Ok(text_value)
                            }
                        }
                    }
                    Err(e) => Err(format!("Tool error: {}", e.message)),
                },
                Err(e) => Err(format!("Dispatch error: {e}")),
            }
        }) as Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>
    })
}

fn format_output(success: bool, return_val: &str, stderr: &str) -> String {
    if success {
        format!("Code Executed Successfully: true\n\n# Return Value\n```json\n{return_val}\n```\n")
    } else {
        format!(
            "Code Executed Successfully: false\n\n# Return Value\n```json\n{return_val}\n```\n\n# STDERR\n{stderr}\n"
        )
    }
}

/// Recursively walk a JSON value, collecting rich content for any
/// `_goose_content_ref` tokens found.
fn collect_rich_content(
    val: &Value,
    store: &HashMap<String, Vec<Content>>,
    rich: &mut Vec<Content>,
) {
    match val {
        Value::Object(map) => {
            if let Some(Value::String(token)) = map.get("_goose_content_ref") {
                if let Some(stored) = store.get(token) {
                    rich.extend(stored.iter().cloned());
                }
            }
            for v in map.values() {
                collect_rich_content(v, store, rich);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_rich_content(v, store, rich);
            }
        }
        _ => {}
    }
}

#[async_trait]
impl McpClientTrait for CodeExecutionClient {
    #[allow(clippy::too_many_lines)]
    async fn list_tools(
        &self,
        _session_id: &str,
        _next_cursor: Option<String>,
        _cancellation_token: CancellationToken,
    ) -> Result<ListToolsResult, Error> {
        fn schema<T: JsonSchema>() -> JsonObject {
            serde_json::to_value(schema_for!(T))
                .map(|v| v.as_object().unwrap().clone())
                .expect("valid schema")
        }

        // Empty schema for list_functions (no parameters)
        let empty_schema: JsonObject = serde_json::from_value(json!({
            "type": "object",
            "properties": {},
            "required": []
        }))
        .expect("valid schema");

        Ok(ListToolsResult {
            tools: vec![
                McpTool::new(
                    "list_functions".to_string(),
                    indoc! {r#"
                        List all available functions across all namespaces.
                        
                        This will not return function input and output types.
                        After determining which functions are needed use
                        get_function_details to get input and output type 
                        information about specific functions.
                    "#}
                    .to_string(),
                    empty_schema,
                )
                .annotate(ToolAnnotations::from_raw(
                    Some("List functions".to_string()),
                    Some(true),
                    Some(false),
                    Some(true),
                    Some(false),
                )),
                McpTool::new(
                    "get_function_details".to_string(),
                    indoc! {r#"
                        Get detailed type information for specific functions.

                        Provide a list of function identifiers in the format "Namespace.functionName"
                        (e.g., "Developer.shell", "Github.createIssue").

                        Returns full TypeScript interface definitions with parameter types,
                        return types, and descriptions for the requested functions.
                    "#}
                    .to_string(),
                    schema::<GetFunctionDetailsInput>(),
                )
                .annotate(ToolAnnotations::from_raw(
                    Some("Get function details".to_string()),
                    Some(true),
                    Some(false),
                    Some(true),
                    Some(false),
                )),
                McpTool::new(
                    "execute".to_string(),
                    indoc! {r#"
                        Execute TypeScript code that calls available functions.

                        SYNTAX - TypeScript with async run() function:
                        ```typescript
                        async function run() {
                            // Access functions via Namespace.functionName({ params }) — always camelCase
                            const files = await Developer.shell({ command: "ls -la" });
                            const readme = await Developer.shell({ command: "cat ./README.md" });
                            return { files, readme };
                        }
                        ```

                        TOOL_GRAPH: Always provide tool_graph to describe the execution flow for the UI.
                        Each node has: tool (Namespace.functionName), description (what it does), depends_on (indices of dependencies).
                        Example for chained operations:
                        [
                          {"tool": "Developer.shell", "description": "list files", "depends_on": []},
                          {"tool": "Developer.shell", "description": "read README.md", "depends_on": []},
                          {"tool": "Developer.write", "description": "write output.txt", "depends_on": [0, 1]}
                        ]

                        KEY RULES:
                        - Code MUST define an async function named `run()`
                        - All function calls are async - use `await`
                        - Function names are always camelCase (e.g., Developer.shell, Github.listIssues, Github.createIssue)
                        - The return value from `run()` is the ONLY output. Do NOT use console.log() — it is ignored.
                        - Only functions from `list_functions()` are available — no `fetch()`, `fs`, or other Node/Deno APIs
                        - Variables don't persist between `execute()` calls - return anything you need later
                        - Code runs in an isolated sandbox with restricted network access

                        HANDLING RETURN VALUES:
                        - If a function returns `any`, do NOT assume its shape - return it directly to inspect the structure
                        - Many functions return wrapper objects, not raw arrays - check the response structure before calling .filter(), .map(), etc.

                        TOKEN USAGE WARNING: This tool could return LARGE responses if your code returns big objects.
                        To minimize tokens:
                        - Filter/map/reduce data IN YOUR CODE before returning
                        - Only return specific fields you need (e.g., return {id: result.id, count: items.length})
                        - Avoid returning full API responses - extract just what you need

                        BEFORE CALLING: Use list_functions or get_function_details to check available functions and their parameters.
                    "#}
                    .to_string(),
                    schema::<ExecuteWithToolGraph>(),
                )
                .annotate(ToolAnnotations::from_raw(
                    Some("Execute TypeScript".to_string()),
                    Some(false),
                    Some(true),
                    Some(false),
                    Some(true),
                )),
            ],
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        session_id: &str,
        name: &str,
        arguments: Option<JsonObject>,
        _working_dir: Option<&str>,
        _cancellation_token: CancellationToken,
    ) -> Result<CallToolResult, Error> {
        let result = match name {
            "list_functions" => self.handle_list_functions(session_id).await,
            "get_function_details" => {
                self.handle_get_function_details(session_id, arguments)
                    .await
            }
            "execute" => self.handle_execute(session_id, arguments).await,
            _ => Err(format!("Unknown tool: {name}")),
        };

        match result {
            Ok(content) => Ok(CallToolResult::success(content)),
            Err(error) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: {error}"
            ))])),
        }
    }

    fn get_info(&self) -> Option<&InitializeResult> {
        Some(&self.info)
    }

    async fn get_moim(&self, session_id: &str) -> Option<String> {
        let code_mode = self.get_code_mode(session_id).await.ok()?;
        let available: Vec<_> = code_mode
            .list_functions()
            .functions
            .iter()
            .map(|f| format!("{}.{}", &f.namespace, &f.name))
            .collect();

        Some(format!(
            indoc::indoc! {r#"
                ALWAYS batch multiple tool operations into ONE execute call.
                - WRONG: Separate execute calls for read file, then write file
                - RIGHT: One execute with an async run() function that reads AND writes

                Available namespaces: {}

                Use the list_functions & get_function_details tools to see tool signatures and input/output types before calling unfamiliar tools.
            "#},
            available.join(", ")
        ))
    }
}

struct CodeModeState {
    code_mode: CodeMode,
    hash: u64,
}

impl CodeModeState {
    fn new(cfgs: Vec<CallbackConfig>) -> Result<Self, String> {
        let hash = Self::hash(&cfgs);

        let code_mode = CodeMode::default()
            .with_callbacks(&cfgs)
            .map_err(|e| format!("failed adding callback configs to CodeMode: {e}"))?;

        Ok(Self { code_mode, hash })
    }

    /// Compute order-independent hash of callback configs
    fn hash(cfgs: &[CallbackConfig]) -> u64 {
        let mut cfg_strings: Vec<_> = cfgs
            .iter()
            .filter_map(|c| serde_json::to_string(c).ok())
            .collect();
        cfg_strings.sort();

        let mut hasher = DefaultHasher::new();
        for s in cfg_strings {
            s.hash(&mut hasher);
        }
        hasher.finish()
    }
}

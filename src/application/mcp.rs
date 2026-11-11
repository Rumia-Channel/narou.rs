//! MCP (Model Context Protocol) server layered on the external API.
//! 外部 API 上に構築された MCP (Model Context Protocol) サーバー。
//!
//! 外部公開 API と同じ読み取り専用データだけを MCP のツールとして出す。
//! ツールは外部 API の条件 (検索・絞り込み・出力形式) をそのまま引数で受け
//! 取り、返り値は API と同じ JSON / JSON Lines / XML。
//!
//! JSON-RPC 2.0 の実装で、HTTP 伝送 (native の `POST /api/v1/mcp` と
//! Worker の同じパス) と stdIO (バイナリ `narou_rs_mcp`) の両方で使う。
//! 通知 (id なし) には応答しない。

use serde_json::{json, Value};

use crate::application::external_api::{
    render, ExternalApiService, ExternalListRequest, OutputFormat, RenderOptions,
};
use crate::hidden::HiddenState;

/// MCP プロトコルのバージョン。
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// サーバーの名前。
pub const SERVER_NAME: &str = "narou.rs";

/// JSON-RPC のエラーコード。
mod error_code {
    /// 実装していないメソッド。
    pub const METHOD_NOT_FOUND: i64 = -32601;
}

/// 外部 API を工具として公開する MCP サーバー。
///
/// The application owns the service; MCP only borrows it for request handling.
pub struct McpServer<'a> {
    api: &'a ExternalApiService,
}

impl<'a> McpServer<'a> {
    pub fn new(api: &'a ExternalApiService) -> Self {
        Self { api }
    }

    /// `initialize` の返り値。
    pub fn initialize_result(&self) -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            }
        })
    }

    /// `tools/list` の返り値。
    pub fn tool_definitions(&self) -> Value {
        json!({ "tools": tool_definitions() })
    }

    /// 1 件の JSON-RPC 要求を処理する。
    ///
    /// 通知 (id なし) は `None` を返す (応答しない)。
    pub async fn handle(&self, request: &Value, hidden: Option<HiddenState>) -> Option<Value> {
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let id = request.get("id").cloned();
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        let result = match method {
            "initialize" => Ok(self.initialize_result()),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.tool_definitions()),
            "tools/call" => self.call_tool(&params, hidden).await,
            _ => {
                // 未知の通知は黙って受け流す (応答しない)。
                id.as_ref()?;
                Err((
                    error_code::METHOD_NOT_FOUND,
                    format!("未対応のメソッドです: {method}"),
                ))
            }
        };

        let id = id?;
        Some(match result {
            Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        })
    }

    async fn call_tool(
        &self,
        params: &Value,
        hidden: Option<HiddenState>,
    ) -> Result<Value, (i64, String)> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
        match self.run_tool(name, &arguments, hidden).await {
            Ok(text) => Ok(tool_result(text)),
            Err(message) => Ok(tool_error(message)),
        }
    }

    async fn run_tool(
        &self,
        name: &str,
        arguments: &Value,
        hidden: Option<HiddenState>,
    ) -> Result<String, String> {
        match name {
            "list_novels" => {
                let request = ExternalListRequest::from_pairs(&argument_pairs(arguments))
                    .map_err(|error| error.message().to_string())?;
                let list = self
                    .api
                    .list(&request, hidden)
                    .await
                    .map_err(|error| error.message().to_string())?;
                let document = list.to_json();
                Ok(render(
                    &document,
                    &RenderOptions {
                        format: request.format,
                        root: "novels",
                        array_names: &[
                            ("data", "novel"),
                            ("tags", "tag"),
                            ("members", "member"),
                        ],
                        xml_item: "novel",
                        jsonl_items: document.get("data").and_then(Value::as_array).map(Vec::as_slice),
                    },
                ))
            }
            "get_novel" => {
                let id = argument_id(arguments)?;
                let novel = self
                    .api
                    .novel(id)
                    .await
                    .map_err(|error| error.message().to_string())?;
                let document = serde_json::to_value(&novel)
                    .map_err(|error| format!("応答を作れません: {error}"))?;
                Ok(render(
                    &document,
                    &RenderOptions {
                        format: OutputFormat::Json,
                        root: "novel",
                        array_names: &[("tags", "tag"), ("members", "member")],
                        xml_item: "novel",
                        jsonl_items: None,
                    },
                ))
            }
            "get_novel_toc" => {
                let id = argument_id(arguments)?;
                let toc = self
                    .api
                    .toc(id)
                    .await
                    .map_err(|error| error.message().to_string())?;
                let document = serde_json::to_value(&toc)
                    .map_err(|error| format!("応答を作れません: {error}"))?;
                Ok(render(
                    &document,
                    &RenderOptions {
                        format: OutputFormat::Json,
                        root: "toc",
                        array_names: &[("sections", "section"), ("members", "member")],
                        xml_item: "section",
                        jsonl_items: None,
                    },
                ))
            }
            "get_novel_section" => {
                let id = argument_id(arguments)?;
                let key = arguments
                    .get("key")
                    .or_else(|| arguments.get("index"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| "key (話番号) を指定してください".to_string())?;
                let section = self
                    .api
                    .section(id, key)
                    .await
                    .map_err(|error| error.message().to_string())?;
                let document = serde_json::to_value(&section)
                    .map_err(|error| format!("応答を作れません: {error}"))?;
                Ok(render(
                    &document,
                    &RenderOptions {
                        format: OutputFormat::Json,
                        root: "section",
                        array_names: &[],
                        xml_item: "section",
                        jsonl_items: None,
                    },
                ))
            }
            "list_tags" => {
                let tags = self
                    .api
                    .tags()
                    .await
                    .map_err(|error| error.message().to_string())?;
                let document = serde_json::to_value(&tags)
                    .map_err(|error| format!("応答を作れません: {error}"))?;
                Ok(render(
                    &document,
                    &RenderOptions {
                        format: OutputFormat::Json,
                        root: "tags",
                        array_names: &[],
                        xml_item: "tag",
                        jsonl_items: None,
                    },
                ))
            }
            other => Err(format!("未対応のツールです: {other}")),
        }
    }
}

/// MCP のツール定義。
pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "list_novels",
            "description": "ライブラリの一覧を返す。条件での絞り込み、並び替え、JSON / JSON Lines / XML の出力形式を選べる。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "q": {
                        "type": "string",
                        "description": "自由検索。tag: / author: / site: / title: / status: が指定でき、-  negation、a|b の OR、引用符による phrases を使う。"
                    },
                    "tag": { "type": "string", "description": "タグ (完全一致)" },
                    "title": { "type": "string", "description": "題名の部分一致" },
                    "author": { "type": "string", "description": "作者名の部分一致" },
                    "site": { "type": "string", "description": "サイト名 (完全一致)" },
                    "ncode": { "type": "string", "description": "ncode (完全一致)" },
                    "attribute": { "type": "string", "enum": ["小説", "漫画"], "description": "属性タグ" },
                    "novel_type": { "type": "integer", "description": "種別 (0 / 1 = 連載、2 = 短編)" },
                    "end": { "type": "boolean", "description": "完結かどうか" },
                    "suspend": { "type": "boolean", "description": "中断かどうか" },
                    "frozen": { "type": "boolean", "description": "凍結かどうか" },
                    "include_hidden": { "type": "boolean", "description": "非表示設定で隠した作品も含めるか" },
                    "virtual": {
                        "type": "string",
                        "enum": ["group", "expand", "exclude"],
                        "description": "バーチャル小説の扱い。group は既定 (1 行にまとめる)、expand はメンバーを個別に出す、exclude はバーチャル小説とメンバーを出さない。"
                    },
                    "sort": { "type": "string", "description": "並び順のキー (id / last_update / title / author / sitename / length など)" },
                    "order": { "type": "string", "enum": ["asc", "desc"] },
                    "offset": { "type": "integer", "description": "開始位置" },
                    "limit": { "type": "integer", "description": "件数上限 (0 または all で無制限)" },
                    "format": { "type": "string", "enum": ["json", "jsonl", "xml"], "description": "出力形式" }
                }
            }
        }),
        json!({
            "name": "get_novel",
            "description": "作品 1 件の情報を返す (題名・作者・タグ・状態・URL)。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "作品 ID" }
                },
                "required": ["id"]
            }
        }),
        json!({
            "name": "get_novel_toc",
            "description": "作品 1 件の目次を返す。バーチャル小説の ID を渡すとメンバーの目次を連結して返す。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "作品 ID (バーチャル小説も可)" }
                },
                "required": ["id"]
            }
        }),
        json!({
            "name": "get_novel_section",
            "description": "作品 1 件の指定した話の本文を返す。key は目次の sections[].key (通常の作品なら話番号、バーチャル小説なら メンバーID:話番号)。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "作品 ID" },
                    "key": { "type": "string", "description": "話番号 (または メンバーID:話番号)" }
                },
                "required": ["id", "key"]
            }
        }),
        json!({
            "name": "list_tags",
            "description": "ライブラリを使っているタグと使用数を返す。",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    ]
}

/// MCP のツール結果を本文だけで返す。
fn tool_result(text: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false
    })
}

/// MCP のツール結果でエラーを返す。
fn tool_error(message: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true
    })
}

/// ツールの引数をクエリパラメータと同じ形にする。
///
/// `null` の引数は「指定なし」として落とす (未指定の値を渡すと
/// 「数値です: 」のようなエラーになる)。
fn argument_pairs(arguments: &Value) -> Vec<(String, String)> {
    match arguments.as_object() {
        Some(map) => map
            .iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| (key.clone(), argument_value(value)))
            .collect(),
        None => Vec::new(),
    }
}

fn argument_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn argument_id(arguments: &Value) -> Result<i64, String> {
    arguments
        .get("id")
        .and_then(|value| match value {
            Value::Number(number) => number.as_i64(),
            Value::String(text) => text.trim().parse::<i64>().ok(),
            _ => None,
        })
        .ok_or_else(|| "id (作品 ID) を指定してください".to_string())
}

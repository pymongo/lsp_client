#![cfg(test)]

use lsp_types::notification::Notification;
use lsp_types::request::Request;

struct ReqId(i32);

impl ReqId {
    fn inc(&mut self) -> lsp_server::RequestId {
        self.0 += 1;
        lsp_server::RequestId::from(self.0)
    }
}

struct Ctx {
    req_to_ra: std::process::ChildStdin,
    rsp_from_ra: std::io::BufReader<std::process::ChildStdout>,
    req_id: ReqId,
}

impl Ctx {
    // https://github.com/rust-lang/rust-analyzer/blob/master/editors/code/src/util.ts#L60
    fn send_req_with_retry(&mut self, req: lsp_server::Request) -> Option<serde_json::Value> {
        for delay_ms in [40, 80, 160, 160, 320, 320, 640, 2560, 10240] {
            let mut req_ = req.clone();
            req_.id = self.req_id.inc();
            let msg = lsp_server::Message::Request(req_);
            msg.write(&mut self.req_to_ra).unwrap();
            let rsp = lsp_server::Message::read(&mut self.rsp_from_ra)
                .unwrap()
                .unwrap()
                .as_resp();
            if let Some(err) = rsp.error {
                // error: waiting for cargo metadata or cargo check
                if err.code != lsp_server::ErrorCode::ContentModified as i32 {
                    panic!("{err:?}");
                }
            } else {
                return rsp.result;
            }
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            // println!("ra is blocking for cargo check, retry delay is {delay_ms}");
        }
        unreachable!("req_to_ra timeout")
    }
}

trait MessageExt {
    fn as_resp(self) -> lsp_server::Response;
}

impl MessageExt for lsp_server::Message {
    fn as_resp(self) -> lsp_server::Response {
        match self {
            lsp_server::Message::Response(resp) => resp,
            _ => unreachable!(),
        }
    }
}

/*
dead_code sample:
```
[workspace]
members = [
    "crates/callee",
    "crates/pub_util",
]

cat crates/pub_util/src/lib.rs
pub fn used_pub() {}
pub fn unused_pub() {}

cat crates/callee/src/main.rs
fn main() {
    pub_util::used_pub();
}
```
*/
#[test]
fn find_dead_code_in_cargo_workspace() {
    let mut lsp_server_process = std::process::Command::new("rust-analyzer")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let req_to_ra = lsp_server_process.stdin.take().unwrap();
    let rsp_from_ra = std::io::BufReader::new(lsp_server_process.stdout.take().unwrap());
    let req_id = ReqId(0);
    let mut ctx = Ctx {
        req_to_ra,
        rsp_from_ra,
        req_id,
    };

    /* LSP server init */
    lsp_server::Message::from(lsp_server::Request {
        id: ctx.req_id.inc(),
        method: <lsp_types::request::Initialize as Request>::METHOD.to_string(),
        params: serde_json::to_value(&lsp_types::InitializeParams {
            root_uri: Some(
                lsp_types::Url::parse("file:///home/w/repos/temp/unused_pub_test_case").unwrap(),
            ),
            ..Default::default()
        })
        .unwrap(),
    })
    .write(&mut ctx.req_to_ra)
    .unwrap();
    // resp of InitializeParams tell which option/feature that LSP server support, we ignore it
    // alternative lsp reader stream parsing https://github.com/rust-lang/rls/blob/master/rls/src/server/io.rs#L40
    let rsp = lsp_server::Message::read(&mut ctx.rsp_from_ra)
        .unwrap()
        .unwrap()
        .as_resp();
    assert!(rsp.error.is_none());
    // send notify_initialized
    lsp_server::Message::from(lsp_server::Notification {
        method: <lsp_types::notification::Initialized as Notification>::METHOD.to_string(),
        params: serde_json::to_value(&lsp_types::InitializedParams {}).unwrap(),
    })
    .write(&mut ctx.req_to_ra)
    .unwrap();

    /* LSP server enter main loop */
    let workspace_symbol_req = lsp_server::Request {
        id: ctx.req_id.inc(),
        method: <rust_analyzer::lsp_ext::WorkspaceSymbol as Request>::METHOD.to_string(),
        params: serde_json::to_value(&rust_analyzer::lsp_ext::WorkspaceSymbolParams {
            search_kind: Some(rust_analyzer::lsp_ext::WorkspaceSymbolSearchKind::AllSymbols),
            work_done_progress_params: lsp_types::WorkDoneProgressParams {
                work_done_token: Some(lsp_types::ProgressToken::Number(ctx.req_id.0)),
            },
            ..Default::default()
        })
        .unwrap(),
    };
    let workspace_symbol_rsp = ctx.send_req_with_retry(workspace_symbol_req).unwrap();
    let workspace_symbol_rsp = serde_json::from_value::<
        <rust_analyzer::lsp_ext::WorkspaceSymbol as Request>::Result,
    >(workspace_symbol_rsp)
    .unwrap();
    // let mut f = std::fs::File::create("target/symbol.log").unwrap();
    for symbol in workspace_symbol_rsp.unwrap() {
        if symbol.kind != lsp_types::SymbolKind::FUNCTION {
            continue;
        }
        if symbol.name == "main" {
            continue;
        }
        let path = symbol.location.uri.to_string();

        let mut p = symbol.location.range.start;
        p.character += "pub fn ".len() as u32 + 1;
        let find_refs_req = lsp_server::Request {
            id: ctx.req_id.inc(),
            method: <lsp_types::request::References as Request>::METHOD.to_string(),
            params: serde_json::to_value(lsp_types::ReferenceParams {
                text_document_position: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: symbol.location.uri,
                    },
                    position: p,
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
                context: lsp_types::ReferenceContext {
                    include_declaration: false,
                },
            })
            .unwrap(),
        };
        let rsp = match ctx.send_req_with_retry(find_refs_req) {
            Some(rsp) => rsp,
            None => {
                println!("References return None");
                continue;
            }
        };
        let rsp = serde_json::from_value::<lsp_types::GotoDefinitionResponse>(rsp).unwrap();
        let refs_cnt = match rsp {
            lsp_types::GotoDefinitionResponse::Scalar(_) => 1,
            lsp_types::GotoDefinitionResponse::Array(arr) => arr.len(),
            lsp_types::GotoDefinitionResponse::Link(arr) => arr.len(),
        };
        if refs_cnt == 0 {
            eprintln!("dead_code found {path} {}", symbol.name);
        }
    }

    /* LSP server exit */
    let exit_req = lsp_server::Request {
        id: ctx.req_id.inc(),
        method: <lsp_types::request::Shutdown as Request>::METHOD.to_string(),
        params: serde_json::Value::Null,
    };
    ctx.send_req_with_retry(exit_req);
    lsp_server::Message::Notification(lsp_server::Notification {
        method: <lsp_types::notification::Exit as Notification>::METHOD.to_string(),
        params: serde_json::Value::Null,
    })
    .write(&mut ctx.req_to_ra)
    .unwrap();
    lsp_server_process.wait().unwrap();
}

use crate::ast::*;
use crate::module::InstanceID;
use crate::semantic::FileID;

use crate::smt::AssertInfo;
use crate::smt::{OwnedSMTModel, SMTModel};
use log::info;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tower_lsp::lsp_types::*;
use tower_lsp::Client;

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum InlaySource {
    None,
    File(FileID),
    Web(u64),
}
#[derive(Clone)]
pub struct InlayHandler {
    source: Arc<Mutex<InlaySource>>,
    tx: mpsc::Sender<InlayEvent>,
}
impl InlayHandler {
    pub fn new(client: Client) -> Self {
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(inlay_handler(rx, client));
        Self {
            source: Arc::new(Mutex::new(InlaySource::None)),
            tx,
        }
    }
    pub fn is_active(&self, source: InlaySource) -> bool {
        *self.source.lock() == source
    }
    pub async fn set_source(&self, source: InlaySource) {
        info!("set {source:?}");
        *self.source.lock() = source;
        let _ = self.tx.send(InlayEvent::SetSource).await;
    }
    pub async fn maybe_publish<F: FnOnce() -> Arc<OwnedSMTModel>>(
        &self,
        source: InlaySource,
        timestamp: Instant,
        f: F,
    ) {
        if *self.source.lock() == source {
            info!("publish");
            let _ = self.tx.send(InlayEvent::Publish(f(), timestamp)).await;
        }
    }
    pub async fn maybe_reset(&self, source: InlaySource) {
        if *self.source.lock() == source {
            info!("reset");
            let _ = self.tx.send(InlayEvent::Reset(Instant::now())).await;
        }
    }
    pub async fn get(&self, uri: &Url, span: Span) -> Option<Vec<InlayHint>> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(InlayEvent::Get(InlayRequest {
                target: FileID::new(uri.as_str()),
                span,
                out: tx,
            }))
            .await;
        rx.await.ok().flatten()
    }
}
struct InlayRequest {
    target: FileID,
    span: Span,
    out: oneshot::Sender<Option<Vec<InlayHint>>>,
}
enum InlayEvent {
    Get(InlayRequest),
    Publish(Arc<OwnedSMTModel>, Instant),
    Reset(Instant),
    SetSource,
}
fn generate(model: &OwnedSMTModel, id: FileID, range: Span) -> Option<Vec<InlayHint>> {
    if !model.modul.ok {
        return None;
    }
    model.modul.files.get(&id).map(|doc| {
        let doc = &doc.content;
        model
            .modul
            .instances()
            .filter(|(_, i)| doc.id == i.id)
            .flat_map(|(m, _)| match &model.model {
                SMTModel::SAT { values, .. } => doc
                    .all_features()
                    .chain(doc.all_attributes())
                    .chain(doc.all_references())
                    .filter(|f| range.contains(&doc.span(*f).unwrap().start))
                    .filter_map(|sym| {
                        let tgt = model.modul.resolve_value(m.sym(sym));
                        let val = values.get(&tgt)?;
                        let range = doc.lsp_range(sym).unwrap();
                        Some(InlayHint {
                            label: InlayHintLabel::String(format!(": {val}")),
                            position: range.end,
                            kind: Some(InlayHintKind::PARAMETER),
                            data: None,
                            padding_left: Some(true),
                            padding_right: Some(true),
                            tooltip: None,
                            text_edits: None,
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter(),
                SMTModel::UNSAT { reasons } => reasons
                    .iter()
                    .filter_map(|AssertInfo(sym, name)| {
                        if id == model.modul.file(sym.instance).id
                            && range.contains(&doc.span(sym.sym).unwrap().start)
                        {
                            let range = doc.lsp_range(sym.sym).unwrap();
                            Some(InlayHint {
                                label: InlayHintLabel::String(format!("UNSAT {}!", name)),
                                position: range.end,
                                kind: Some(InlayHintKind::PARAMETER),
                                data: None,
                                padding_left: Some(true),
                                padding_right: Some(true),
                                tooltip: None,
                                text_edits: None,
                            })
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .into_iter(),
            })
            .collect()
    })
}
async fn inlay_handler(mut rx: mpsc::Receiver<InlayEvent>, client: Client) {
    let mut map: Option<Arc<OwnedSMTModel>> = None;
    let mut latest = Instant::now();
    let mut initial = false;
    while let Some(e) = rx.recv().await {
        match e {
            InlayEvent::Get(request) => {
                info!("get");
                if let Some(model) = map.as_ref() {
                    let _ = request
                        .out
                        .send(generate(model, request.target, request.span));
                } else {
                    let _ = request.out.send(None);
                }
                info!("done");
            }
            InlayEvent::Reset(timestamp) => {
                if timestamp <= latest {
                    continue;
                }
                latest = timestamp;
                map = None;
                client
                    .send_request::<tower_lsp::lsp_types::request::InlayHintRefreshRequest>(())
                    .await
                    .unwrap();
            }
            InlayEvent::Publish(model, timestamp) => {
                if timestamp <= latest {
                    continue;
                }
                latest = timestamp;
                if initial {
                    let file = model.modul.file(InstanceID(0));
                    let _ = client
                        .show_document(ShowDocumentParams {
                            uri: file.uri.clone(),
                            external: Some(false),
                            take_focus: Some(true),
                            selection: Some(Range::default()),
                        })
                        .await;
                    //Force VS-Code to refresh inlays since inlay-hints-refresh is sometimes ingored
                    //When the document had no previous inlays
                    //Currently done via a pseudo edit(TODO this sucks)
                    //Insert a '0'
                    let changes = [(
                        file.uri.clone(),
                        vec![TextEdit::new(
                            Range {
                                start: Position::default(),
                                end: Position {
                                    line: 0,
                                    character: 0,
                                },
                            },
                            "1".into(),
                        )],
                    )];

                    client
                        .send_request::<tower_lsp::lsp_types::request::InlayHintRefreshRequest>(())
                        .await
                        .unwrap();
                    let _ = client
                        .send_request::<tower_lsp::lsp_types::request::ApplyWorkspaceEdit>(
                            ApplyWorkspaceEditParams {
                                label: None,
                                edit: WorkspaceEdit {
                                    changes: Some(changes.into()),
                                    document_changes: None,
                                    change_annotations: None,
                                },
                            },
                        )
                        .await;

                    //Remove it
                    let changes = [(
                        file.uri.clone(),
                        vec![TextEdit::new(
                            Range {
                                start: Position::default(),
                                end: Position {
                                    line: 0,
                                    character: 1,
                                },
                            },
                            "".into(),
                        )],
                    )];
                    let _ = client
                        .send_request::<tower_lsp::lsp_types::request::ApplyWorkspaceEdit>(
                            ApplyWorkspaceEditParams {
                                label: None,
                                edit: WorkspaceEdit {
                                    changes: Some(changes.into()),
                                    document_changes: None,
                                    change_annotations: None,
                                },
                            },
                        )
                        .await;

                    info!("focus");
                    initial = false;
                }
                map = Some(model);

                client
                    .send_request::<tower_lsp::lsp_types::request::InlayHintRefreshRequest>(())
                    .await
                    .unwrap();
            }

            InlayEvent::SetSource => {
                initial = true;
                info!("set source");
                map = None;
                client
                    .send_request::<tower_lsp::lsp_types::request::InlayHintRefreshRequest>(())
                    .await
                    .unwrap();
            }
        }
    }
}

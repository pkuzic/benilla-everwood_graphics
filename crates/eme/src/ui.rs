//! The UI shell (`eme-ui`) wired to [`EmeHost`]: ONE tool registry and ONE undo stack, the
//! shell only draws them.
//!
//! - toolbar = `registry.visible(dispatcher.permissions(), dispatcher.caps())`, options = the
//!   active tool's `options_ui_model()`, History = `dispatcher.undo_stack()`, the confirmation
//!   modal = an entry in `NeedsConfirm` (the dispatcher's `UndoNeedsConfirm`);
//! - [`EmeUiCommand`]s become [`EmeRequest`]s (undo / redo / confirm / cancel / tool / option) or
//!   service requests (restore points, log paging), applied by the dispatch frame;
//! - replies fill [`EmeRestorePoints`] / [`EmeLogTable`]; the status bar is fed from the voxel
//!   seam and the link.
//!
//! The host owns the egui camera / context (and `EguiPlugin`, added before [`crate::EmePlugin`]
//! when it wants its own overlay camera) and sets [`EmeLanguage`].

use bevy::prelude::*;
use eme_core::service::{EmeLogPage, EmeLogQuery, EmeLogRow, EmeSnapshotRow};
use eme_core::{EmeServiceRequest, EmeToolRegistry, EmeUndoStack};
use eme_ui::{
    EmeI18n, EmeLang, EmeLogTable, EmeRestorePoints, EmeRightTab, EmeSession, EmeShell,
    EmeShellSource, EmeStatus, EmeUiCommand, EmeUiPlugin,
};

use crate::camera::EmeFreeCam;
use crate::{EmeActive, EmeHost, EmeRequest, EmeSet, EmeWire};
use benilla_assets::coords::bevy_to_wow;
use benilla_world::voxel::MonkeyVoxelApi;

/// Log rows per page (CONTRACT `log_rows_max` is 15 by default).
const LOG_PAGE_ROWS: u32 = 15;
/// Restore points listed.
const RESTORE_ROWS: u32 = 25;

impl EmeShellSource for EmeHost {
    fn registry(&self) -> &EmeToolRegistry {
        &self.registry
    }
    fn undo_stack(&self) -> &EmeUndoStack {
        self.dispatcher.undo_stack()
    }
}

/// The log browser's paging cursors: `before[n]` = the `before_seq` that fetches page `n`
/// (`None` = the newest page).
#[derive(Resource, Debug, Default)]
pub struct EmeLogPaging {
    before: Vec<Option<u32>>,
    /// The page the last request asked for (applied when its reply lands).
    asked: u32,
}

pub(crate) fn register(app: &mut App) {
    app.add_plugins(EmeUiPlugin::<EmeHost>::default())
        .init_resource::<EmeLogPaging>()
        .add_systems(Startup, extend_locales)
        .add_systems(Update, ui_commands.in_set(EmeSet::Input))
        .add_systems(
            Update,
            (ui_replies, ui_status, refresh_on_open)
                .chain()
                .after(EmeSet::Frame),
        );
}

/// The built-in tools' option labels (`MONKEY_EME_SCULPT_*`), both languages.
fn extend_locales(mut i18n: ResMut<EmeI18n>) {
    for (lang, json) in [
        (EmeLang::EnUs, include_str!("../locales/enUS.json")),
        (EmeLang::RuRu, include_str!("../locales/ruRU.json")),
    ] {
        if let Err(e) = i18n.extend_json(lang, json) {
            error!("EME: bad built-in locale {lang:?}: {e}");
        }
    }
}

/// The request for a log page.
fn log_request(before_seq: Option<u32>) -> EmeServiceRequest {
    EmeServiceRequest::Log(EmeLogQuery {
        limit: Some(LOG_PAGE_ROWS),
        before_seq,
        ..Default::default()
    })
}

fn snapshot_list() -> EmeServiceRequest {
    EmeServiceRequest::SnapshotList {
        radius: None,
        limit: Some(RESTORE_ROWS),
    }
}

/// Shell commands → EME requests.
fn ui_commands(
    mut cmds: MessageReader<EmeUiCommand>,
    mut out: MessageWriter<EmeRequest>,
    host: Res<EmeHost>,
    log: Res<EmeLogTable>,
    mut paging: ResMut<EmeLogPaging>,
) {
    for c in cmds.read() {
        let req = match c {
            EmeUiCommand::ActivateTool(id) => EmeRequest::SelectTool(id.as_str().to_owned()),
            EmeUiCommand::Undo => EmeRequest::Undo,
            EmeUiCommand::Redo => EmeRequest::Redo,
            EmeUiCommand::ConfirmUndo(id) => EmeRequest::ConfirmUndo(*id),
            EmeUiCommand::CancelConfirm(id) => EmeRequest::CancelConfirm(*id),
            EmeUiCommand::SetOption { tool, key, value } => {
                // Only the active tool's panel is drawn; a stale frame's click is dropped.
                if host.registry.active_id() != Some(*tool) {
                    continue;
                }
                EmeRequest::SetOption {
                    key: key.clone(),
                    value: value.clone(),
                }
            }
            EmeUiCommand::Service(r) => EmeRequest::Service(r.clone()),
            EmeUiCommand::LogPage { older } => {
                let page = log.page;
                let (want, before) = if *older {
                    let cursor = log.rows.iter().map(|r| r.seq).min();
                    let Some(cursor) = cursor else { continue };
                    paging.before.truncate(page as usize + 1);
                    paging.before.push(Some(cursor));
                    (page + 1, Some(cursor))
                } else if page > 0 {
                    let b = paging.before.get(page as usize - 1).copied().flatten();
                    (page - 1, b)
                } else {
                    continue;
                };
                paging.asked = want;
                EmeRequest::Service(log_request(before))
            }
            EmeUiCommand::TabOpened(EmeRightTab::Restore) => EmeRequest::Service(snapshot_list()),
            EmeUiCommand::TabOpened(EmeRightTab::Log) => {
                paging.before = vec![None];
                paging.asked = 0;
                EmeRequest::Service(log_request(None))
            }
            EmeUiCommand::TabOpened(EmeRightTab::History) => continue,
        };
        out.write(req);
    }
}

/// Service replies for the panels.
fn ui_replies(
    host: Res<EmeHost>,
    mut restore: ResMut<EmeRestorePoints>,
    mut log: ResMut<EmeLogTable>,
    paging: Res<EmeLogPaging>,
    mut out: MessageWriter<EmeRequest>,
) {
    for r in host.replies.iter().filter(|r| r.tool.is_none()) {
        if !r.reply.ok {
            info!("EME: {} refused: {:?}", r.op, r.reply.err);
            continue;
        }
        match r.op {
            "snapshot_list" => match r.reply.rows_as::<EmeSnapshotRow>() {
                Ok(rows) => restore.rows = rows,
                Err(e) => warn!("EME: snapshot_list rows: {e}"),
            },
            "log" => {
                let rows = r.reply.rows_as::<EmeLogRow>();
                let page = r.reply.extra_as::<EmeLogPage>().unwrap_or_default();
                match rows {
                    Ok(rows) => {
                        *log = EmeLogTable {
                            count: page.count.max(rows.len() as u32),
                            more: page.more || r.reply.more(),
                            page: paging.asked,
                            rows,
                        }
                    }
                    Err(e) => warn!("EME: log rows: {e}"),
                }
            }
            // A new restore point or a revert changes the list.
            "snapshot_create" | "revert" => {
                out.write(EmeRequest::Service(snapshot_list()));
            }
            _ => {}
        }
    }
}

/// The status bar and the session (rank, caps, link).
fn ui_status(
    host: Res<EmeHost>,
    wire: Res<EmeWire>,
    api: MonkeyVoxelApi,
    cam: Res<EmeFreeCam>,
    time: Res<Time>,
    mut session: ResMut<EmeSession>,
    mut status: ResMut<EmeStatus>,
    mut rate: Local<(u64, f32, f32)>,
) {
    let next = EmeSession {
        perms: host.dispatcher.permissions(),
        caps: host.dispatcher.caps().cloned(),
        link: wire.link,
    };
    if session.perms != next.perms || session.caps != next.caps || session.link != next.link {
        *session = next;
    }
    // Uplink rate: edits sent over a ~1 s exponential window.
    let dt = time.delta_secs().max(1e-4);
    let (last, ref mut acc, ref mut r) = *rate;
    let sent = wire.sent_total.saturating_sub(last) as f32;
    *acc += sent;
    let k = (dt / 1.0).min(1.0);
    *r += (*acc / dt - *r) * k;
    *acc = 0.0;
    rate.0 = wire.sent_total;
    let map = api.map().unwrap_or(0);
    let eye = bevy_to_wow(cam.pos);
    let (rx, ry) = ((eye[0] / 64.0).floor() as i16, (eye[1] / 64.0).floor() as i16);
    let region = api.region_state(map, rx, ry);
    let next = EmeStatus {
        map,
        region: format!("{rx},{ry}"),
        epoch: u64::from(region.sync.epoch),
        pending: wire.awaiting.len() as u32,
        remesh_ms: (api.stream_ms() * 10.0).round() / 10.0,
        rate: (rate.2 * 10.0).round() / 10.0,
    };
    if *status != next {
        *status = next;
    }
}

/// Opening EME (or opening a tab) fetches that tab's rows.
fn refresh_on_open(
    active: Res<EmeActive>,
    shell: Res<EmeShell>,
    mut was: Local<bool>,
    mut paging: ResMut<EmeLogPaging>,
    mut out: MessageWriter<EmeRequest>,
) {
    if active.0 && !*was {
        match shell.layout.right_tab {
            EmeRightTab::Restore => {
                out.write(EmeRequest::Service(snapshot_list()));
            }
            EmeRightTab::Log => {
                paging.before = vec![None];
                paging.asked = 0;
                out.write(EmeRequest::Service(log_request(None)));
            }
            EmeRightTab::History => {}
        }
    }
    *was = active.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every option label of every built-in tool exists in both languages.
    #[test]
    fn built_in_tool_labels_exist_in_both_languages() {
        let mut i18n = EmeI18n::builtin();
        i18n.extend_json(EmeLang::EnUs, include_str!("../locales/enUS.json"))
            .unwrap();
        i18n.extend_json(EmeLang::RuRu, include_str!("../locales/ruRU.json"))
            .unwrap();
        let tool = crate::tools::sculpt::EmeSculptTool::new(eme_core::EmeStrokeIds::default());
        let mut keys: Vec<String> = Vec::new();
        for f in eme_core::EmeTool::options_ui_model(&tool) {
            keys.push(f.label.clone());
            if let eme_core::EmeOptionKind::Choice(c) = &f.kind {
                keys.extend(c.iter().cloned());
            }
        }
        keys.push("MONKEY_EME_SCULPT_NAME".into());
        for lang in [EmeLang::EnUs, EmeLang::RuRu] {
            for k in &keys {
                assert!(i18n.has(lang, k), "{lang:?}: missing {k}");
            }
        }
        for k in &keys {
            assert_ne!(
                i18n.get(EmeLang::EnUs, k),
                i18n.get(EmeLang::RuRu, k),
                "{k}: RU equals EN"
            );
        }
    }
}

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

use std::collections::HashMap;

use bevy::prelude::*;
use eme_core::service::{
    EmeLogPage, EmeLogQuery, EmeLogRow, EmeOthersEdited, EmeRevertResult, EmeServiceError,
    EmeSnapshotRow,
};
use eme_core::{
    EmeProjectInfo, EmeProjectRevertResult, EmeProjectRow, EmeServiceRequest, EmeToolId,
    EmeToolRegistry, EmeUndoStack,
};
use eme_ui::{
    EmeI18n, EmeLang, EmeLogTable, EmeNotice, EmeOtherStroke, EmeOthersHistory,
    EmeProjectConflict, EmeProjects, EmeRestorePoints, EmeRightTab, EmeSession, EmeShell,
    EmeShellSource, EmeStatus, EmeUiCommand, EmeUiPlugin,
};

use crate::camera::EmeFreeCam;
use crate::viewport::{EmeMarker, MARKER_MS};
use crate::{now_ms, EmeActive, EmeHost, EmeRequest, EmeSet, EmeWire};
use benilla_assets::coords::bevy_to_wow;
use benilla_world::voxel::MonkeyVoxelApi;

/// MONKEY (EME projects): projects listed per page.
const PROJECT_ROWS: u32 = 15;
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
    /// The filters in force (the log tab's Apply).
    query: EmeLogQuery,
    before: Vec<Option<u32>>,
    /// The page the last request asked for (applied when its reply lands).
    asked: u32,
}

/// The pseudo-tool the character-name lookups are filed under (their replies are not the log
/// table's).
const NAMES: EmeToolId = EmeToolId("names");
/// Most other-GM stroke rows kept.
const OTHERS_MAX: usize = 30;
/// A stroke row keeps absorbing edits of the same (actor, stroke) for this long, ms.
const OTHERS_MERGE_MS: u64 = 60_000;
/// Notices fade after this long, ms.
const NOTICE_MS: u64 = 20_000;

/// Low GUID → character name, learned from log rows.
#[derive(Resource, Debug, Default)]
pub struct EmeNames {
    names: HashMap<u32, String>,
    asked_ms: u64,
}

/// MONKEY (EME projects): the project the last `project_revert` was for (its `others_edited`
/// reply does not name it).
#[derive(Resource, Debug, Default)]
pub struct EmeProjectAsks {
    revert: Option<u32>,
}

pub(crate) fn register(app: &mut App) {
    app.add_plugins(EmeUiPlugin::<EmeHost>::default())
        .init_resource::<EmeLogPaging>()
        .init_resource::<EmeNames>()
        .init_resource::<EmeProjectAsks>()
        .add_systems(Startup, extend_locales)
        .add_systems(Update, ui_commands.in_set(EmeSet::Input))
        .add_systems(
            Update,
            (
                ui_replies,
                ui_status,
                refresh_on_open,
                others_names,
                notice_expiry,
            )
                .chain()
                .after(EmeSet::Frame),
        );
}

/// A live stroke edit by another character: add it to the read-only History rows.
pub(crate) fn note_remote(
    others: &mut EmeOthersHistory,
    actor: u32,
    stroke: u16,
    mode: u8,
    pos: [f32; 3],
    now: u64,
) {
    if let Some(i) = others.rows.iter().position(|r| {
        r.actor == actor && r.stroke == stroke && now.saturating_sub(r.seen_ms) < OTHERS_MERGE_MS
    }) {
        let mut r = others.rows.remove(i);
        r.edits += 1;
        r.pos = pos;
        r.seen_ms = now;
        others.rows.insert(0, r);
    } else {
        others.rows.insert(
            0,
            EmeOtherStroke {
                actor,
                name: String::new(),
                stroke,
                edits: 1,
                mode,
                pos,
                seen_ms: now,
            },
        );
        others.rows.truncate(OTHERS_MAX);
    }
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

/// The request for a log page of `query` (the filters), `before_seq` = the paging cursor.
fn log_request(query: &EmeLogQuery, before_seq: Option<u32>) -> EmeServiceRequest {
    EmeServiceRequest::Log(EmeLogQuery {
        limit: Some(LOG_PAGE_ROWS),
        before_seq,
        ..query.clone()
    })
}

fn snapshot_list() -> EmeServiceRequest {
    EmeServiceRequest::SnapshotList {
        radius: None,
        limit: Some(RESTORE_ROWS),
    }
}

/// Shell commands → EME requests.
#[allow(clippy::too_many_arguments)]
fn ui_commands(
    mut cmds: MessageReader<EmeUiCommand>,
    mut out: MessageWriter<EmeRequest>,
    host: Res<EmeHost>,
    log: Res<EmeLogTable>,
    mut paging: ResMut<EmeLogPaging>,
    mut restore: ResMut<EmeRestorePoints>,
    mut notice: ResMut<EmeNotice>,
    (mut cam, mut marker, time, api): (
        ResMut<EmeFreeCam>,
        ResMut<EmeMarker>,
        Res<Time>,
        MonkeyVoxelApi,
    ),
    (mut projects, mut asks): (ResMut<EmeProjects>, ResMut<EmeProjectAsks>),
) {
    for c in cmds.read() {
        let req = match c {
            // MONKEY (EME projects).
            EmeUiCommand::ProjectFilter(f) => {
                projects.filter = *f;
                EmeRequest::Service(f.request(PROJECT_ROWS))
            }
            EmeUiCommand::DismissProjectConflict => {
                projects.conflict = None;
                continue;
            }
            EmeUiCommand::TabOpened(EmeRightTab::Projects) => {
                EmeRequest::Service(projects.filter.request(PROJECT_ROWS))
            }
            EmeUiCommand::Service(r @ EmeServiceRequest::ProjectRevert { project, .. }) => {
                asks.revert = Some(*project);
                EmeRequest::Service(r.clone())
            }
            EmeUiCommand::LogQuery(q) => {
                paging.query = q.clone();
                paging.before = vec![None];
                paging.asked = 0;
                EmeRequest::Service(log_request(&paging.query, None))
            }
            EmeUiCommand::SelectRestore(id) => {
                restore.selected = *id;
                continue;
            }
            EmeUiCommand::DismissNotice => {
                notice.clear();
                continue;
            }
            EmeUiCommand::ShowInWorld { x, y, z } => {
                show_in_world(&mut cam, &mut marker, &api, now_ms(&time), [*x, *y], *z);
                continue;
            }
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
                EmeRequest::Service(log_request(&paging.query, before))
            }
            EmeUiCommand::TabOpened(EmeRightTab::Restore) => EmeRequest::Service(snapshot_list()),
            EmeUiCommand::TabOpened(EmeRightTab::Log) => {
                paging.before = vec![None];
                paging.asked = 0;
                EmeRequest::Service(log_request(&paging.query, None))
            }
            EmeUiCommand::TabOpened(EmeRightTab::History) => continue,
        };
        out.write(req);
    }
}

/// The notice for a refused service reply: the server's `msg` key if the client knows it, else
/// the code's own key, else the generic "refused: <code>".
fn error_notice(i18n: &EmeI18n, reply: &eme_core::EmeServiceReply) -> (String, Vec<String>) {
    let code = reply.err.clone().unwrap_or_else(|| "internal".to_owned());
    let err = EmeServiceError::parse(&code);
    let known = |k: &str| i18n.has(EmeLang::EnUs, k) && i18n.has(EmeLang::RuRu, k);
    let key = reply
        .msg
        .clone()
        .filter(|k| known(k))
        .unwrap_or_else(|| err.locale_key());
    if known(&key) && !matches!(err, EmeServiceError::Other(_)) {
        (key, Vec::new())
    } else {
        ("MONKEY_EME_ERR_OTHER".to_owned(), vec![code])
    }
}

/// Service replies for the panels.
#[allow(clippy::too_many_arguments)]
fn ui_replies(
    mut host: ResMut<EmeHost>,
    mut restore: ResMut<EmeRestorePoints>,
    mut log: ResMut<EmeLogTable>,
    paging: Res<EmeLogPaging>,
    mut names: ResMut<EmeNames>,
    mut notice: ResMut<EmeNotice>,
    i18n: Res<EmeI18n>,
    mut out: MessageWriter<EmeRequest>,
    mut seen_errors: Local<Vec<eme_core::EmeDispatchError>>,
    (mut projects, asks, mut shell): (ResMut<EmeProjects>, Res<EmeProjectAsks>, ResMut<EmeShell>),
) {
    // Local refusals (link down, not permitted) and host-queued messages.
    if host.errors != *seen_errors {
        *seen_errors = host.errors.clone();
        if seen_errors
            .iter()
            .any(|e| matches!(e.error, eme_core::EmeSendError::LinkDown))
        {
            notice.set("MONKEY_EME_NOTICE_NOT_SENT", Vec::new(), true);
        }
        // MONKEY (EME projects): a brush used without a project.
        if seen_errors
            .iter()
            .any(|e| matches!(e.error, eme_core::EmeSendError::NoProject))
        {
            host.prompt_project = true;
        }
    }
    // MONKEY (EME projects): `R 7` / `no_project` / a local refusal → say so and show the tab.
    if std::mem::take(&mut host.prompt_project) {
        notice.set("MONKEY_EME_NOTICE_NO_PROJECT", Vec::new(), true);
        shell.layout.right_open = true;
        if shell.layout.right_tab != EmeRightTab::Projects {
            shell.layout.right_tab = EmeRightTab::Projects;
            out.write(EmeRequest::Service(projects.filter.request(PROJECT_ROWS)));
        }
    }
    if std::mem::take(&mut host.projects_dirty) {
        out.write(EmeRequest::Service(projects.filter.request(PROJECT_ROWS)));
    }
    for (key, args, error) in std::mem::take(&mut host.notices) {
        notice.set(key, args, error);
    }
    for r in host.replies.iter() {
        // Name lookups (filed under the pseudo-tool) feed the cache only.
        if r.tool == Some(NAMES) {
            if let (true, Ok(rows)) = (r.reply.ok, r.reply.rows_as::<EmeLogRow>()) {
                for row in rows {
                    names.names.insert(row.guid, row.name);
                }
            }
            continue;
        }
        if r.tool.is_some() {
            continue;
        }
        if !r.reply.ok {
            info!("EME: {} refused: {:?}", r.op, r.reply.err);
            // MONKEY (EME projects): other GMs' work in the project → the force / cancel modal.
            if r.op == "project_revert" && r.reply.err.as_deref() == Some("others_edited") {
                let o = r.reply.extra_as::<EmeOthersEdited>().unwrap_or_default();
                let project = asks.revert.unwrap_or(0);
                let name = projects
                    .rows
                    .iter()
                    .find(|p| p.id == project)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| format!("#{project}"));
                projects.conflict = Some(EmeProjectConflict {
                    project,
                    name,
                    others: o.others,
                    count: o.count,
                });
                continue;
            }
            if r.reply.err.as_deref() == Some("no_project") {
                continue; // the prompt above says it
            }
            if !matches!(r.op, "view" | "caps") {
                let (key, args) = error_notice(&i18n, &r.reply);
                notice.set(key, args, true);
            }
            continue;
        }
        match r.op {
            // MONKEY (EME projects).
            "project_list" => match r.reply.rows_as::<EmeProjectRow>() {
                Ok(rows) => projects.rows = rows,
                Err(e) => warn!("EME: project_list rows: {e}"),
            },
            "project_info" => match r.reply.extra_as::<EmeProjectInfo>() {
                Ok(info) => projects.info = Some(info),
                Err(e) => warn!("EME: project_info: {e}"),
            },
            "project_create" | "project_open" => {
                let name = host
                    .dispatcher
                    .project()
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                notice.set("MONKEY_EME_NOTICE_PROJECT_OPEN", vec![name], false);
                out.write(EmeRequest::Service(projects.filter.request(PROJECT_ROWS)));
            }
            "project_leave" => {
                notice.set("MONKEY_EME_NOTICE_PROJECT_LEFT", Vec::new(), false);
                out.write(EmeRequest::Service(projects.filter.request(PROJECT_ROWS)));
            }
            "project_close" => {
                notice.set("MONKEY_EME_NOTICE_PROJECT_CLOSED", Vec::new(), false);
                out.write(EmeRequest::Service(projects.filter.request(PROJECT_ROWS)));
            }
            "project_revert" | "project_unrevert" => {
                let n = r.reply.extra_as::<EmeProjectRevertResult>().unwrap_or_default();
                let key = if r.op == "project_revert" {
                    "MONKEY_EME_NOTICE_PROJECT_REVERTED"
                } else {
                    "MONKEY_EME_NOTICE_PROJECT_UNREVERTED"
                };
                notice.set(
                    key,
                    vec![n.voxel.to_string(), n.liquid.to_string(), n.area.to_string()],
                    false,
                );
                if projects.info.as_ref().is_some_and(|i| i.project.id == n.project_id) {
                    out.write(EmeRequest::Service(EmeServiceRequest::ProjectInfo {
                        project: n.project_id,
                    }));
                }
                out.write(EmeRequest::Service(projects.filter.request(PROJECT_ROWS)));
                out.write(EmeRequest::Service(log_request(&paging.query, None)));
            }
            "snapshot_list" => match r.reply.rows_as::<EmeSnapshotRow>() {
                Ok(rows) => {
                    restore.rows = rows;
                    if restore
                        .selected
                        .is_some_and(|id| !restore.rows.iter().any(|r| r.snapshot == id))
                    {
                        restore.selected = None;
                    }
                }
                Err(e) => warn!("EME: snapshot_list rows: {e}"),
            },
            "log" => {
                let rows = r.reply.rows_as::<EmeLogRow>();
                let page = r.reply.extra_as::<EmeLogPage>().unwrap_or_default();
                match rows {
                    Ok(rows) => {
                        for row in &rows {
                            names.names.insert(row.guid, row.name.clone());
                        }
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
            "snapshot_create" => {
                notice.set("MONKEY_EME_NOTICE_SNAPSHOT_OK", Vec::new(), false);
                out.write(EmeRequest::Service(snapshot_list()));
            }
            // A revert changes the list and the log (its rows are now marked reverted).
            "revert" | "undo_player" => {
                let n = r.reply.extra_as::<EmeRevertResult>().unwrap_or_default();
                let key = if r.op == "revert" {
                    "MONKEY_EME_NOTICE_REVERT_OK"
                } else {
                    "MONKEY_EME_NOTICE_UNDOPLAYER_OK"
                };
                notice.set(
                    key,
                    vec![n.voxel.to_string(), n.liquid.to_string(), n.area.to_string()],
                    false,
                );
                out.write(EmeRequest::Service(snapshot_list()));
                out.write(EmeRequest::Service(log_request(&paging.query, None)));
            }
            _ => {}
        }
    }
}

/// Jump the free camera to a spot and mark it. `z` is looked up from the ground when unknown.
fn show_in_world(
    cam: &mut EmeFreeCam,
    marker: &mut EmeMarker,
    api: &MonkeyVoxelApi,
    now: u64,
    xy: [f32; 2],
    z: Option<f32>,
) {
    let eye_now = bevy_to_wow(cam.pos);
    let z = z
        .or_else(|| api.surface_z(xy[0], xy[1], eye_now[2], 400.0))
        .unwrap_or(eye_now[2] - 10.0);
    // Stand back south-west and above, looking at the spot.
    let target = [xy[0], xy[1], z];
    cam.look_at_wow([xy[0] - 14.0, xy[1] - 6.0, z + 11.0], target);
    *marker = EmeMarker {
        pos: target,
        until_ms: now + MARKER_MS,
    };
}

/// Give the other-GM rows their names (from the log cache), ask for unknown ones (rarely).
fn others_names(
    time: Res<Time>,
    active: Res<EmeActive>,
    mut host: ResMut<EmeHost>,
    mut wire: ResMut<EmeWire>,
    mut names: ResMut<EmeNames>,
    mut others: ResMut<EmeOthersHistory>,
) {
    let now = now_ms(&time);
    others.now_ms = now;
    let mut unknown = false;
    for r in others.rows.iter_mut().filter(|r| r.name.is_empty()) {
        match names.names.get(&r.actor) {
            Some(n) => r.name = n.clone(),
            None => unknown = true,
        }
    }
    if unknown && active.0 && wire.link.is_usable() && now.saturating_sub(names.asked_ms) > 5_000 {
        names.asked_ms = now;
        let req = EmeServiceRequest::Log(EmeLogQuery {
            minutes: Some(10),
            radius: Some(500.0),
            limit: Some(25),
            ..Default::default()
        });
        let host = &mut *host;
        let _ = host.dispatcher.request(now, &mut *wire, Some(NAMES), req);
    }
}

/// A notice fades after a while (results sooner than errors).
fn notice_expiry(
    time: Res<Time>,
    mut notice: ResMut<EmeNotice>,
    mut shown: Local<(u32, u64)>,
) {
    if notice.key.is_empty() {
        return;
    }
    let now = now_ms(&time);
    if shown.0 != notice.seq {
        *shown = (notice.seq, now);
    }
    let life = if notice.error { NOTICE_MS } else { NOTICE_MS / 2 };
    if now.saturating_sub(shown.1) > life {
        notice.clear();
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
        project: host.dispatcher.project().cloned(),
        projects_required: host.dispatcher.projects_required(),
    };
    if session.perms != next.perms
        || session.caps != next.caps
        || session.link != next.link
        || session.project != next.project
        || session.projects_required != next.projects_required
    {
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
    projects: Res<EmeProjects>,
) {
    if active.0 && !*was {
        match shell.layout.right_tab {
            EmeRightTab::Restore => {
                out.write(EmeRequest::Service(snapshot_list()));
            }
            EmeRightTab::Log => {
                paging.before = vec![None];
                paging.asked = 0;
                out.write(EmeRequest::Service(log_request(&paging.query, None)));
            }
            EmeRightTab::History => {}
            EmeRightTab::Projects => {
                out.write(EmeRequest::Service(projects.filter.request(PROJECT_ROWS)));
            }
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
        let ids = eme_core::EmeStrokeIds::default();
        let sculpt = crate::tools::sculpt::EmeSculptTool::new(ids.clone());
        let flatten = crate::tools::flatten::EmeFlattenTool::new(ids.clone());
        let pushpull = crate::tools::pushpull::EmePushPullTool::new(ids.clone());
        let bridge = crate::tools::bridge::EmeBridgeTool::new(ids.clone());
        let waterfill = crate::tools::waterfill::EmeWaterFillTool::new(ids);
        // MONKEY (EME v3.6): the new tools' notices.
        let mut keys: Vec<String> = [
            "BRIDGE_PICK_B", "BRIDGE_READY", "BRIDGE_SENDING", "BRIDGE_TOO_LONG", "BRIDGE_TOO_MANY",
            "BRIDGE_CANCELLED", "WATERFILL_PREVIEW", "WATERFILL_OPEN", "WATERFILL_TOO_BIG",
            "WATERFILL_SHALLOW", "WATERFILL_NO_GROUND", "WATERFILL_DONE", "WATERFILL_CANCELLED",
        ]
        .iter()
        .map(|k| format!("MONKEY_EME_{k}"))
        .collect();
        for tool in [
            &sculpt as &dyn eme_core::EmeTool,
            &flatten as &dyn eme_core::EmeTool,
            &pushpull as &dyn eme_core::EmeTool,
            &bridge as &dyn eme_core::EmeTool,
            &waterfill as &dyn eme_core::EmeTool,
        ] {
            for f in tool.options_ui_model() {
                keys.push(f.label.clone());
                if let eme_core::EmeOptionKind::Choice(c) = &f.kind {
                    keys.extend(c.iter().cloned());
                }
            }
            keys.push(format!("{}NAME", tool.id().locale_prefix()));
        }
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

    /// Other GMs' strokes: one row per (actor, stroke), newest first, edits counted.
    #[test]
    fn remote_edits_group_into_stroke_rows() {
        let mut o = EmeOthersHistory::default();
        note_remote(&mut o, 9, 4, 1, [1.0, 2.0, 3.0], 1_000);
        note_remote(&mut o, 9, 4, 1, [2.0, 2.0, 3.0], 1_200);
        note_remote(&mut o, 7, 1, 0, [0.0, 0.0, 0.0], 1_500);
        assert_eq!(o.rows.len(), 2);
        assert_eq!((o.rows[0].actor, o.rows[0].edits), (7, 1));
        assert_eq!((o.rows[1].actor, o.rows[1].edits), (9, 2));
        assert_eq!(o.rows[1].pos, [2.0, 2.0, 3.0]);
        // The same stroke id much later is a new stroke (ids restart per session).
        note_remote(&mut o, 9, 4, 1, [0.0; 3], 1_200 + OTHERS_MERGE_MS + 1);
        assert_eq!(o.rows.len(), 3);
        for n in 0..(OTHERS_MAX as u16 + 5) {
            note_remote(&mut o, 100, n, 0, [0.0; 3], 5_000_000);
        }
        assert_eq!(o.rows.len(), OTHERS_MAX);
    }
}

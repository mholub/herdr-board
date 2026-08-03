use super::*;
use board_core::db::BOARD_ID;
use board_core::protocol::{BoardChangedReason, TemplateApplyParams};
use board_core::template::{definition, DEFAULT_TEMPLATE_ALIAS};
pub(super) fn daemon_status(d: &Arc<Daemon>) -> Result<Value> {
    let (active_runs, queued_runs) = {
        let db = d.store.lock();
        (db.count_active_runs()?, db.count_queued_runs()?)
    };
    let herdr_connected = match &d.herdr {
        Some(h) => {
            let mut c = h.clone();
            c.is_live()
        }
        None => false,
    };
    Ok(json!(DaemonStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        db_path: d.db_path.to_string_lossy().into_owned(),
        herdr_connected,
        active_runs,
        queued_runs,
    }))
}

fn board_snapshot(d: &Arc<Daemon>, board_id: i64) -> Result<Value> {
    let db = d.store.lock();
    Ok(json!(BoardSnapshot {
        board: db.get_board(board_id)?,
        columns: db.list_columns(board_id)?,
        cards: db.list_cards(board_id)?,
        active_runs: db.active_run_summaries(board_id)?,
    }))
}

pub(super) fn board_open(d: &Arc<Daemon>, p: BoardOpenParams) -> Result<Value> {
    let board = d.store.lock().open_board(&p.scope_path)?;
    board_snapshot(d, board.id)
}

pub(super) fn board_list(d: &Arc<Daemon>) -> Result<Value> {
    Ok(json!(BoardListResult {
        boards: d.store.lock().list_boards()?,
    }))
}

pub(super) fn board_rename(d: &Arc<Daemon>, p: BoardRenameParams) -> Result<Value> {
    let board = d.store.lock().rename_board(p.board_id, &p.name)?;
    // There is no board-renamed reason in protocol v1. ColumnChanged is the
    // existing board-structure refresh signal and, unlike the legacy coarse
    // emit, scopes the refresh to the renamed board.
    d.emit_changed_board(BoardChangedReason::ColumnChanged, board.id, None, None);
    Ok(json!(board))
}

pub(super) fn board_get(d: &Arc<Daemon>, p: BoardGetParams) -> Result<Value> {
    board_snapshot(d, p.board_id.unwrap_or(BOARD_ID))
}

/// Apply a built-in template as one DB unit of work. Preparation and the
/// post-commit event are outside the transaction; no dispatcher wake is needed
/// because a template creates no cards or runs.
pub(super) fn template_apply(d: &Arc<Daemon>, p: TemplateApplyParams) -> Result<Value> {
    let requested_name = p.name;
    let name = if requested_name == DEFAULT_TEMPLATE_ALIAS {
        d.config.default_template.as_str()
    } else {
        requested_name.as_str()
    };
    let board_id = p.board_id.unwrap_or(BOARD_ID);
    let columns = {
        let _sched = d.sched.lock().unwrap();
        let db = d.store.lock();
        db.get_board(board_id)?;
        let existing = db.list_columns(board_id)?;
        let cards = db.list_cards(board_id)?;
        if existing.len() != 1 || existing[0].name != "Todo" || !cards.is_empty() {
            return Err(Error::InvalidState(
                "template.apply requires an empty board (only the seed Todo column, no cards)"
                    .into(),
            ));
        }
        let todo = existing[0].id;
        let template = definition(name, board_id, todo)
            .ok_or_else(|| Error::BadRequest(format!("unknown template: {name}")))?;
        db.apply_template_columns_uow(
            board_id,
            &template.columns,
            &template.wiring,
            template.seed_name,
        )?
    };
    d.emit_changed_board(BoardChangedReason::ColumnChanged, board_id, None, None);
    Ok(json!(columns))
}

// -- columns ----------------------------------------------------------------

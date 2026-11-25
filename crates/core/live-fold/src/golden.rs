use serde_json::json;

use crate::{
    Ack, ClientFrame, Cursor, ErrCode, Failure, Intent, Op, Row, ServerFrame, ViewKey, encode,
};

pub fn golden_frames() -> Vec<(String, String)> {
    let view = ViewKey::new("thread")
        .arg("thread", "t1")
        .arg("project", "p1");
    let from = Cursor::new(3, 7, 41);
    let to = Cursor::new(3, 7, 45);
    let mut frames = client_frames(view, from);
    frames.extend(data_frames(from, to));
    frames.extend(reply_frames(to));
    frames.extend(error_frames());
    frames
}

fn client_frames(view: ViewKey, from: Cursor) -> Vec<(String, String)> {
    vec![
        ("view_key".into(), view.canonical()),
        (
            "sub_cursor".into(),
            encode(&ClientFrame::Sub {
                id: 1,
                view,
                cursor: Some(from),
            }),
        ),
        (
            "sub_fresh".into(),
            encode(&ClientFrame::Sub {
                id: 2,
                view: ViewKey::new("inbox"),
                cursor: None,
            }),
        ),
        ("unsub".into(), encode(&ClientFrame::Unsub { id: 1 })),
        (
            "mut".into(),
            encode(&ClientFrame::Mut(Intent {
                client: "8f14e45f-ceea-467f-a0e6-3b1c2a9d0e11".into(),
                mid: 12,
                name: "rename_thread".into(),
                args: json!({"thread": "t1", "title": "Ship it \"now\" é"}),
            })),
        ),
    ]
}

fn data_frames(from: Cursor, to: Cursor) -> Vec<(String, String)> {
    vec![
        (
            "snap".into(),
            encode(&ServerFrame::Snap {
                id: 1,
                cursor: from,
                rows: vec![
                    Row {
                        table: "messages".into(),
                        key: "m1".into(),
                        value: json!({"body": "hi", "n": 1, "sealed": false, "tags": ["a", "b"], "x": null}),
                    },
                    Row {
                        table: "threads".into(),
                        key: "t1".into(),
                        value: json!({"title": "Plan", "score": 1.5}),
                    },
                ],
            }),
        ),
        (
            "diff".into(),
            encode(&ServerFrame::Diff {
                id: 1,
                from,
                to,
                ops: vec![
                    Op::Upsert {
                        table: "threads".into(),
                        key: "t1".into(),
                        value: json!({"title": "Plan B"}),
                    },
                    Op::Delete {
                        table: "messages".into(),
                        key: "m1".into(),
                    },
                ],
            }),
        ),
        (
            "diff_cursor_only".into(),
            encode(&ServerFrame::Diff {
                id: 1,
                from: to,
                to: Cursor::new(3, 8, 52),
                ops: Vec::new(),
            }),
        ),
    ]
}

fn reply_frames(to: Cursor) -> Vec<(String, String)> {
    vec![
        (
            "ack_ok".into(),
            encode(&ServerFrame::Ack(Ack {
                client: "8f14e45f-ceea-467f-a0e6-3b1c2a9d0e11".into(),
                mid: 12,
                result: Ok(to),
            })),
        ),
        (
            "ack_error".into(),
            encode(&ServerFrame::Ack(Ack {
                client: "8f14e45f-ceea-467f-a0e6-3b1c2a9d0e11".into(),
                mid: 13,
                result: Err(Failure {
                    code: "not_found".into(),
                    message: "no thread t9".into(),
                }),
            })),
        ),
        (
            "eph".into(),
            encode(&ServerFrame::Eph {
                id: 1,
                body: json!({"kind": "token", "text": "Hel"}),
            }),
        ),
    ]
}

fn error_frames() -> Vec<(String, String)> {
    let mut frames = Vec::new();
    for (name, code) in [
        ("err_denied", ErrCode::Denied),
        ("err_unknown_view", ErrCode::UnknownView),
        ("err_overloaded", ErrCode::Overloaded),
    ] {
        frames.push((
            name.into(),
            encode(&ServerFrame::Err {
                id: Some(2),
                code,
                message: "refused".into(),
            }),
        ));
    }
    frames.push((
        "err_bad_frame".into(),
        encode(&ServerFrame::Err {
            id: None,
            code: ErrCode::BadFrame,
            message: "expected a frame".into(),
        }),
    ));
    frames
}

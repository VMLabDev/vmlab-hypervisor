use std::io::{BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;
use std::time::Duration;

use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::codec;
use vmlab_hv_proto::control::{Capability, Command, ErrorCode, Event, Outcome, ServerMsg};
use vmlab_hv_proto::server::Server;

#[test]
fn old_client_new_server_keeps_reading_after_unknown_additions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        codec::read_client_msg(&mut reader).unwrap().unwrap();
        writeln!(stream, r#"{{"type":"hello","proto_version":1,"hv_version":"future","capabilities":["snapshots","future_capability"],"future_field":true}}"#).unwrap();
        codec::read_client_msg(&mut reader).unwrap().unwrap();
        writeln!(
            stream,
            r#"{{"type":"event","event":"future_event","payload":{{"nested":true}}}}"#
        )
        .unwrap();
        writeln!(
            stream,
            r#"{{"type":"event","event":"reset","future_field":42}}"#
        )
        .unwrap();
        writeln!(
            stream,
            r#"{{"type":"reply","id":1,"outcome":"ok","future_field":42}}"#
        )
        .unwrap();
        writeln!(stream, r#"{{"type":"event","event":"wakeup"}}"#).unwrap();
    });
    let mut client = Client::connect(path).unwrap();
    assert!(client.has_capability(Capability::Snapshots));
    assert!(!client.has_capability(Capability::Display));
    assert_eq!(
        serde_json::to_value(&client.capabilities()[1]).unwrap(),
        "future_capability"
    );
    assert_eq!(
        client.call(Command::Status).unwrap(),
        Outcome::Ok { status: None }
    );
    let events = client.take_events();
    assert_eq!(
        serde_json::to_value(&events[0]).unwrap()["event"],
        "future_event"
    );
    assert_eq!(events[1], Event::Reset);
    assert_eq!(client.wait_event().unwrap(), Some(Event::Wakeup));
    assert_eq!(client.wait_event().unwrap(), None);
    worker.join().unwrap();
}

#[test]
fn new_client_old_server_defaults_optional_fields() {
    let hello: ServerMsg =
        serde_json::from_str(r#"{"type":"hello","proto_version":1,"hv_version":"old"}"#).unwrap();
    assert!(matches!(hello, ServerMsg::Hello { capabilities, .. } if capabilities.is_empty()));
    let exiting: ServerMsg = serde_json::from_str(
        r#"{"type":"event","event":"exiting","reason":{"kind":"quit"},"code":0}"#,
    )
    .unwrap();
    assert!(
        matches!(exiting, ServerMsg::Event { event: Event::Exiting { error_chain, .. } } if error_chain.is_empty())
    );
}

#[test]
fn unknown_error_code_stays_an_error_with_its_wire_name() {
    for wire in [
        r#"{"type":"reply","id":1,"outcome":"error","code":"future_error","message":"refused","detail":{}}"#,
        r#"{"type":"error","code":"future_error","message":"refused","detail":{}}"#,
    ] {
        let msg: ServerMsg = serde_json::from_str(wire).unwrap();
        let code = match msg {
            ServerMsg::Reply {
                outcome: Outcome::Error { code, .. },
                ..
            }
            | ServerMsg::Error { code, .. } => code,
            other => panic!("error was lost: {other:?}"),
        };
        assert_eq!(serde_json::to_value(code).unwrap(), "future_error");
    }
}

#[test]
fn unsupported_request_is_replied_to_without_calling_handler_or_closing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = Server::new("test", vec![], |command| {
        assert_eq!(command, Command::Status);
        Outcome::Ok { status: None }
    });
    let owner = server.clone();
    let worker = thread::spawn(move || server.serve(listener).unwrap());
    let mut stream = UnixStream::connect(path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    writeln!(stream, r#"{{"type":"hello","proto_version":1}}"#).unwrap();
    assert!(matches!(
        codec::read_server_msg(&mut reader).unwrap(),
        Some(ServerMsg::Hello { .. })
    ));
    writeln!(
        stream,
        r#"{{"type":"request","id":41,"command":{{"command":"future_request","parameter":true}}}}"#
    )
    .unwrap();
    let reply = codec::read_server_msg(&mut reader);
    if matches!(reply, Ok(Some(_))) {
        writeln!(
            stream,
            r#"{{"type":"request","id":42,"command":{{"command":"status"}}}}"#
        )
        .unwrap();
        assert_eq!(
            codec::read_server_msg(&mut reader).unwrap(),
            Some(ServerMsg::Reply {
                id: 42,
                outcome: Outcome::Ok { status: None },
            })
        );
    }
    owner.shutdown();
    worker.join().unwrap();
    assert!(matches!(reply.unwrap(), Some(ServerMsg::Reply {
        id: 41, outcome: Outcome::Error { code: ErrorCode::Unsupported, message }
    }) if message.contains("unsupported request")));
}

#[test]
fn malformed_known_messages_are_not_unknown_extensions() {
    for wire in [
        r#"{"type":"event","event":"shutdown"}"#,
        r#"{"type":"event","event":"device_deleted","id":17}"#,
        r#"{"type":"reply","id":1,"outcome":"ok","status":false}"#,
        r#"{"type":"event","event":17}"#,
    ] {
        assert!(
            serde_json::from_str::<ServerMsg>(wire).is_err(),
            "accepted {wire}"
        );
    }
    assert!(
        serde_json::from_str::<vmlab_hv_proto::control::ClientMsg>(
            r#"{"type":"request","id":1,"command":{"command":"snapshot_save"}}"#,
        )
        .is_err()
    );
}

use vmlab_hv_proto::control::{ClientMsg, Command, Event, Outcome, RunState, ServerMsg};
use vmlab_hv_proto::debug::{AddressSpace, DebugRequest, DebugResponse, DebugStop, StopReason};

#[test]
fn debugger_request_result_and_stop_event_keep_distinct_tags() {
    let request = ClientMsg::Request {
        id: 7,
        command: Command::Debug {
            request: DebugRequest::ReadMemory {
                space: AddressSpace::Virtual { vcpu: 2 },
                address: 0x8000,
                length: 16,
            },
        },
    };
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(wire["command"]["request"]["operation"], "read_memory");
    assert_eq!(serde_json::from_value::<ClientMsg>(wire).unwrap(), request);
    let response = ServerMsg::Reply {
        id: 7,
        outcome: Outcome::Debug {
            response: DebugResponse::Memory { data: vec![0, 255] },
        },
    };
    let wire = serde_json::to_value(&response).unwrap();
    assert_eq!(wire["outcome"], "debug");
    assert_eq!(wire["response"]["result"], "memory");
    assert_eq!(serde_json::from_value::<ServerMsg>(wire).unwrap(), response);
    let event = ServerMsg::Event {
        event: Event::DebugStopped {
            stop: DebugStop {
                sequence: 4,
                vcpu: 2,
                pc: 0x8000,
                reason: StopReason::Step,
            },
        },
    };
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["event"], "debug_stopped");
    assert_eq!(wire["stop"]["reason"], "step");
    assert_eq!(serde_json::from_value::<ServerMsg>(wire).unwrap(), event);
    assert_eq!(serde_json::to_value(RunState::Paused).unwrap(), "paused");
}

#[test]
fn malformed_debugger_requests_and_stop_events_are_not_unknown_extensions() {
    for wire in [
        r#"{"type":"request","id":1,"command":{"command":"debug","request":{"operation":"read_memory","address":0}}}"#,
        r#"{"type":"request","id":1,"command":{"command":"debug","request":{"operation":"step","vcpu":256}}}"#,
    ] {
        assert!(serde_json::from_str::<ClientMsg>(wire).is_err());
    }
    for wire in [
        r#"{"type":"event","event":"debug_stopped"}"#,
        r#"{"type":"event","event":"debug_stopped","stop":{}}"#,
    ] {
        assert!(serde_json::from_str::<ServerMsg>(wire).is_err());
    }
}

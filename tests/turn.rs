use herdr_mcp::herdr::AgentInfo;
use herdr_mcp::turn::{Turn, TurnFrom, Verdict, evaluate};
use serde_json::json;

fn agent(status: &str, seq: u64, completion: Option<u64>, terminal: &str) -> AgentInfo {
    serde_json::from_value(json!({
        "terminal_id": terminal, "name": "rev", "agent": "codex", "agent_status": status,
        "pane_id": "w1:p4", "tab_id": "w1:t2", "workspace_id": "w1", "focused": false,
        "revision": 1, "state_change_seq": seq, "completion_seq": completion,
    }))
    .unwrap()
}

fn turn(after: u64, from: TurnFrom) -> Turn {
    Turn {
        pane: "w1:p4".into(),
        terminal_id: "t-1".into(),
        shell_pid: Some(100),
        after,
        from,
    }
}

fn rebased(after: u64) -> Verdict {
    Verdict::Rebase(turn(after, TurnFrom::Working))
}

fn matched(superseded: bool) -> Verdict {
    Verdict::Matched { superseded }
}

struct Case {
    name: &'static str,
    turn: Turn,
    agent: AgentInfo,
    shell_pid: Option<u32>,
    want: Verdict,
}

#[test]
fn ktd6_matching_table() {
    let cases = vec![
        Case {
            name: "completion above after",
            turn: turn(41, TurnFrom::Working),
            agent: agent("done", 43, Some(43), "t-1"),
            shell_pid: Some(100),
            want: matched(false),
        },
        Case {
            name: "completion above after wins over a later working status",
            turn: turn(41, TurnFrom::Working),
            agent: agent("working", 45, Some(43), "t-1"),
            shell_pid: Some(100),
            want: matched(false),
        },
        Case {
            name: "completion at after does not count",
            turn: turn(43, TurnFrom::Working),
            agent: agent("working", 43, Some(43), "t-1"),
            shell_pid: Some(100),
            want: Verdict::Pending,
        },
        Case {
            name: "idle above after without completion",
            turn: turn(41, TurnFrom::Working),
            agent: agent("idle", 42, None, "t-1"),
            shell_pid: Some(100),
            want: matched(false),
        },
        Case {
            name: "unknown above after settles",
            turn: turn(41, TurnFrom::Working),
            agent: agent("unknown", 43, None, "t-1"),
            shell_pid: Some(100),
            want: matched(false),
        },
        Case {
            name: "blocked above after matches",
            turn: turn(41, TurnFrom::Working),
            agent: agent("blocked", 42, None, "t-1"),
            shell_pid: Some(100),
            want: matched(false),
        },
        Case {
            name: "settled status at after is still the activity that started the turn",
            turn: turn(41, TurnFrom::Working),
            agent: agent("idle", 41, None, "t-1"),
            shell_pid: Some(100),
            want: Verdict::Pending,
        },
        Case {
            name: "working at after keeps waiting",
            turn: turn(41, TurnFrom::Working),
            agent: agent("working", 41, None, "t-1"),
            shell_pid: Some(100),
            want: Verdict::Pending,
        },
        Case {
            name: "working again above after is superseded",
            turn: turn(41, TurnFrom::Working),
            agent: agent("working", 45, None, "t-1"),
            shell_pid: Some(100),
            want: matched(true),
        },
        Case {
            name: "from blocked, still blocked at after",
            turn: turn(42, TurnFrom::Blocked),
            agent: agent("blocked", 42, None, "t-1"),
            shell_pid: Some(100),
            want: Verdict::Pending,
        },
        Case {
            name: "from blocked, working above after re-bases",
            turn: turn(42, TurnFrom::Blocked),
            agent: agent("working", 43, None, "t-1"),
            shell_pid: Some(100),
            want: rebased(43),
        },
        Case {
            name: "from blocked, done with completion matches",
            turn: turn(42, TurnFrom::Blocked),
            agent: agent("done", 44, Some(44), "t-1"),
            shell_pid: Some(100),
            want: matched(false),
        },
        Case {
            name: "from blocked, blocked again above after matches",
            turn: turn(42, TurnFrom::Blocked),
            agent: agent("blocked", 46, None, "t-1"),
            shell_pid: Some(100),
            want: matched(false),
        },
        Case {
            name: "new terminal id is stale",
            turn: turn(41, TurnFrom::Working),
            agent: agent("done", 43, Some(43), "t-2"),
            shell_pid: Some(100),
            want: Verdict::Stale,
        },
        Case {
            name: "changed shell pid is stale",
            turn: turn(41, TurnFrom::Working),
            agent: agent("done", 43, Some(43), "t-1"),
            shell_pid: Some(101),
            want: Verdict::Stale,
        },
        Case {
            name: "unreadable shell pid is not stale",
            turn: turn(41, TurnFrom::Working),
            agent: agent("done", 43, Some(43), "t-1"),
            shell_pid: None,
            want: matched(false),
        },
        Case {
            name: "token without a shell pid is not stale",
            turn: Turn {
                shell_pid: None,
                ..turn(41, TurnFrom::Working)
            },
            agent: agent("done", 43, Some(43), "t-1"),
            shell_pid: Some(555),
            want: matched(false),
        },
        Case {
            name: "restart resets the counter, stale beats a low seq",
            turn: turn(41, TurnFrom::Working),
            agent: agent("idle", 2, None, "t-2"),
            shell_pid: Some(100),
            want: Verdict::Stale,
        },
    ];
    for case in cases {
        assert_eq!(
            evaluate(&case.turn, &case.agent, case.shell_pid),
            case.want,
            "{}",
            case.name
        );
    }
}

#[test]
fn token_from_a_working_or_blocked_snapshot() {
    let working = Turn::from_agent(&agent("working", 41, None, "t-1"), Some(100)).unwrap();
    assert_eq!(working, turn(41, TurnFrom::Working));
    let blocked = Turn::from_agent(&agent("blocked", 42, None, "t-1"), Some(100)).unwrap();
    assert_eq!(blocked, turn(42, TurnFrom::Blocked));
    assert!(Turn::from_agent(&agent("idle", 42, None, "t-1"), Some(100)).is_none());
}

#[test]
fn token_round_trips_as_json() {
    let token = turn(41, TurnFrom::Blocked);
    let value = serde_json::to_value(&token).unwrap();
    assert_eq!(
        value,
        json!({"pane": "w1:p4", "terminal_id": "t-1", "shell_pid": 100, "after": 41, "from": "blocked"})
    );
    assert_eq!(serde_json::from_value::<Turn>(value).unwrap(), token);
}

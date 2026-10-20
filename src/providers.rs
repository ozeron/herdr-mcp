pub struct Provider {
    pub kind: &'static str,
    pub interrupt_key: &'static str,
    pub interrupt_note: Option<&'static str>,
}

pub const PROCESS_INTERRUPT_KEY: &str = "ctrl+c";

const UNVERIFIED: Option<&str> =
    Some("interrupt key not verified for this agent kind; Esc assumed");

const FALLBACK: Provider = Provider {
    kind: "",
    interrupt_key: "esc",
    interrupt_note: UNVERIFIED,
};

const PROVIDERS: &[Provider] = &[
    Provider {
        kind: "claude",
        interrupt_key: "esc",
        interrupt_note: None,
    },
    Provider {
        kind: "codex",
        interrupt_key: "esc",
        interrupt_note: None,
    },
];

pub fn provider(kind: Option<&str>) -> &'static Provider {
    kind.and_then(|kind| PROVIDERS.iter().find(|p| p.kind == kind))
        .unwrap_or(&FALLBACK)
}

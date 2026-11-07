//! The gateway's own session model (plan 09 step 6): one session per
//! gateway token, aged out after the configured TTL. The MCP protocol
//! (revision 2026-07-28) defines no sessions; this is the gateway's
//! accounting view of its clients, not a protocol session.

use std::collections::HashMap;

use time::OffsetDateTime;

/// One live gateway session: the token id that owns it and when it was
/// last seen.
#[derive(Debug, Clone)]
pub struct GatewaySession {
    /// The gateway token id (`mcp-token-<n>`).
    pub token_id: String,
    /// First seen (RFC 3339 when rendered).
    pub created_at: OffsetDateTime,
    /// Last request from this session.
    pub last_seen: OffsetDateTime,
}

/// The live sessions.
#[derive(Debug, Default)]
pub struct SessionTable {
    sessions: HashMap<String, GatewaySession>,
}

impl SessionTable {
    /// Drops sessions idle longer than `ttl`.
    pub fn expire(&mut self, ttl: std::time::Duration) {
        let now = OffsetDateTime::now_utc();
        let ttl = time::Duration::milliseconds(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX));
        self.sessions
            .retain(|_, session| now - session.last_seen < ttl);
    }

    /// The session for `token_id`, created on first sight and refreshed
    /// on every sight.
    pub fn touch(&mut self, token_id: &str) -> GatewaySession {
        let now = OffsetDateTime::now_utc();
        let session = self
            .sessions
            .entry(token_id.to_owned())
            .or_insert_with(|| GatewaySession {
                token_id: token_id.to_owned(),
                created_at: now,
                last_seen: now,
            });
        session.last_seen = now;
        session.clone()
    }

    /// Number of live sessions (observable).
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether no session is live.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_created_once_and_refreshed() {
        let mut table = SessionTable::default();
        let first = table.touch("mcp-token-0");
        assert_eq!(table.len(), 1);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = table.touch("mcp-token-0");
        assert_eq!(table.len(), 1, "the same token reuses its session");
        assert!(second.last_seen >= first.last_seen);
        assert_eq!(second.created_at, first.created_at);
    }

    #[test]
    fn idle_sessions_expire() {
        let mut table = SessionTable::default();
        table.touch("mcp-token-0");
        assert_eq!(table.len(), 1);
        // Touch a *different* session far in the past by hand: expire()
        // compares against now, so use a zero TTL instead.
        table.expire(std::time::Duration::ZERO);
        assert!(table.is_empty(), "a zero TTL ages everything out");
    }
}

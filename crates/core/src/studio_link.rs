//! Commands for the Rojo Manager bridges in Studio. A bridge polls Rojo Manager without pause: each
//! poll brings the result of the command the previous one handed it, and is held until the next
//! command for that bridge, or a few seconds.

use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

/// One command for a bridge, as its poll receives it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Command {
    pub id: u64,
    pub action: String,
    pub args: Value,
}

#[derive(Default)]
pub struct Link {
    inner: Mutex<Inner>,
    wake: Condvar,
}

#[derive(Default)]
struct Inner {
    last_id: u64,
    /// Commands not yet handed out, by bridge.
    queued: HashMap<u64, VecDeque<Command>>,
    /// Commands someone waits on, with their bridge and their result once it arrives.
    results: HashMap<u64, (u64, Option<Result<Value, String>>)>,
}

impl Link {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Hand a command to `bridge` on its next poll and wait for the result it reports.
    pub fn run(&self, bridge: u64, action: &str, args: Value, timeout: Duration) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;
        let mut inner = self.lock();
        inner.last_id += 1;
        let id = inner.last_id;
        inner.queued.entry(bridge).or_default().push_back(Command { id, action: action.into(), args });
        inner.results.insert(id, (bridge, None));
        self.wake.notify_all();
        loop {
            if let Some(result) = inner.results.get_mut(&id).and_then(|(_, result)| result.take()) {
                inner.results.remove(&id);
                return result;
            }
            let now = Instant::now();
            if now >= deadline {
                inner.results.remove(&id);
                if let Some(queue) = inner.queued.get_mut(&bridge) {
                    queue.retain(|command| command.id != id);
                }
                return Err("Studio did not answer in time.".into());
            }
            inner = self.wake.wait_timeout(inner, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// A poll from `bridge`: record the result it brings, then wait up to `hold` for its next command.
    pub fn poll(&self, bridge: u64, result: Option<(u64, Result<Value, String>)>, hold: Duration) -> Option<Command> {
        let deadline = Instant::now() + hold;
        let mut inner = self.lock();
        if let Some((id, result)) = result
            && let Some((_, slot)) = inner.results.get_mut(&id)
        {
            *slot = Some(result);
            self.wake.notify_all();
        }
        loop {
            if let Some(command) = inner.queued.get_mut(&bridge).and_then(VecDeque::pop_front) {
                return Some(command);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            inner = self.wake.wait_timeout(inner, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Whether someone still waits on a command for `bridge`. A bridge runs a command before it polls
    /// again, so a long one is not a sign that its Studio closed.
    pub fn busy(&self, bridge: u64) -> bool {
        self.lock().results.values().any(|(waiting, result)| *waiting == bridge && result.is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn a_held_poll_receives_the_command_and_its_result_reaches_the_caller() {
        let link = Arc::new(Link::default());
        let bridge = {
            let link = Arc::clone(&link);
            std::thread::spawn(move || {
                let command = link.poll(7, None, Duration::from_secs(5)).unwrap();
                assert_eq!((command.action.as_str(), &command.args), ("status", &json!({})));
                link.poll(7, Some((command.id, Ok(json!({"status": "Connected"})))), Duration::ZERO)
            })
        };
        let result = link.run(7, "status", json!({}), Duration::from_secs(5));
        assert_eq!(result, Ok(json!({"status": "Connected"})));
        assert_eq!(bridge.join().unwrap(), None);
    }

    #[test]
    fn a_bridge_is_busy_while_its_command_runs() {
        let link = Arc::new(Link::default());
        let caller = {
            let link = Arc::clone(&link);
            std::thread::spawn(move || link.run(7, "luau", json!({}), Duration::from_secs(5)))
        };
        let command = link.poll(7, None, Duration::from_secs(5)).unwrap();
        assert!(link.busy(7) && !link.busy(8));
        link.poll(7, Some((command.id, Err("boom".into()))), Duration::ZERO);
        assert_eq!(caller.join().unwrap(), Err("boom".into()));
        assert!(!link.busy(7));
    }

    #[test]
    fn an_unanswered_command_times_out_and_is_withdrawn() {
        let link = Link::default();
        assert_eq!(
            link.run(7, "status", json!({}), Duration::from_millis(50)).unwrap_err(),
            "Studio did not answer in time."
        );
        assert_eq!(link.poll(7, None, Duration::ZERO), None);
    }
}

use super::*;

type PollerHeartbeats = BTreeMap<(String, String), chrono::DateTime<chrono::Utc>>;

#[derive(Debug, Clone, Default)]
pub(crate) struct ReleaseActionStore {
    actions: Arc<Mutex<BTreeMap<String, ReleaseAction>>>,
    next: Arc<std::sync::atomic::AtomicU64>,
    pollers: Arc<Mutex<PollerHeartbeats>>,
}

impl ReleaseActionStore {
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ReleaseAction>> {
        self.actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records an ask, or answers the open ask for the same step. The `bool`
    /// says whether this call recorded it.
    pub(super) fn record(
        &self,
        new: NewAction,
        by: &str,
        now: String,
    ) -> Result<(ReleaseAction, bool), &'static str> {
        let mut actions = self.lock();
        Self::expire(&mut actions, &now);
        if let Some(existing) = actions.values().find(|a| a.open() && a.same_step(&new)) {
            return Ok((existing.clone(), false));
        }
        if actions.len() >= MAX_ACTIONS {
            // Forget the oldest finished ask; an open one is never dropped.
            if let Some(oldest) = actions
                .values()
                .filter(|a| !a.open())
                .min_by(|a, b| a.requested_at.cmp(&b.requested_at))
                .map(|a| a.id.clone())
            {
                actions.remove(&oldest);
            } else {
                return Err(
                    "release action queue is full; cancel a pending ask or wait for an outcome",
                );
            }
        }
        // Time-ordered and unique within this forge's life, which is the
        // store's life.
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let stamp: String = now.chars().filter(char::is_ascii_alphanumeric).collect();
        let id = format!("ra-{stamp}-{n}");
        let action = ReleaseAction {
            id: id.clone(),
            action: new.action,
            repo: new.repo,
            release: new.release,
            head: new.head,
            state: ActionState::Pending,
            requested_by: by.to_string(),
            requested_at: now,
            claimed_by: None,
            claimed_at: None,
            finished_at: None,
            message: None,
            log_tail: None,
        };
        actions.insert(id, action.clone());
        Ok((action, true))
    }

    pub(super) fn expire(actions: &mut BTreeMap<String, ReleaseAction>, now: &str) {
        let Ok(now_time) = chrono::DateTime::parse_from_rfc3339(now) else {
            return;
        };
        for action in actions.values_mut().filter(|action| action.open()) {
            let (since, minutes) = if action.state == ActionState::Claimed {
                (action.claimed_at.as_deref(), CLAIMED_TTL_MINUTES)
            } else {
                (Some(action.requested_at.as_str()), PENDING_TTL_MINUTES)
            };
            if since
                .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
                .is_some_and(|since| now_time - since >= chrono::Duration::minutes(minutes))
            {
                let claimed = action.state == ActionState::Claimed;
                action.state = ActionState::Expired;
                action.finished_at = Some(now.to_string());
                action.message = Some(if claimed {
                    "Release host outcome unknown after 90 minutes. Check the host and deployment records before requesting another step."
                } else { "Not claimed within 15 minutes; no step was run." }.to_string());
            }
        }
    }

    pub(super) fn all_at(&self, now: &str) -> Vec<ReleaseAction> {
        let mut actions = self.lock();
        Self::expire(&mut actions, now);
        actions.values().cloned().collect()
    }

    pub(super) fn cancel(
        &self,
        id: &str,
        now: String,
    ) -> Result<ReleaseAction, Option<ActionState>> {
        let mut actions = self.lock();
        Self::expire(&mut actions, &now);
        let action = actions.get_mut(id).ok_or(None)?;
        if action.state != ActionState::Pending {
            return Err(Some(action.state));
        }
        action.state = ActionState::Cancelled;
        action.finished_at = Some(now);
        action.message = Some("Cancelled before the release host claimed it.".into());
        Ok(action.clone())
    }

    pub(super) fn check_in(
        &self,
        repo: &str,
        by: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), &'static str> {
        let mut pollers = self
            .pollers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pollers.retain(|_, stamp| now - *stamp < chrono::Duration::seconds(POLLER_TTL_SECONDS));
        let key = (repo.to_string(), by.to_string());
        if !pollers.contains_key(&key) && pollers.len() >= MAX_ACTIONS {
            return Err("at most 256 authenticated release pollers may report");
        }
        pollers.insert(key, now);
        Ok(())
    }

    pub(super) fn poller_repos(&self, now: chrono::DateTime<chrono::Utc>) -> Vec<String> {
        let mut pollers = self
            .pollers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pollers.retain(|_, stamp| now - *stamp < chrono::Duration::seconds(POLLER_TTL_SECONDS));
        pollers
            .keys()
            .map(|(repo, _)| repo.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(super) fn poller_repos_for(
        &self,
        by: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Vec<String> {
        let mut pollers = self
            .pollers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pollers.retain(|_, stamp| now - *stamp < chrono::Duration::seconds(POLLER_TTL_SECONDS));
        pollers
            .keys()
            .filter(|(_, reporter)| reporter == by)
            .map(|(repo, _)| repo.clone())
            .collect()
    }

    pub(crate) fn all(&self) -> Vec<ReleaseAction> {
        self.lock().values().cloned().collect()
    }

    pub(super) fn claim(
        &self,
        id: &str,
        by: &str,
        now: String,
    ) -> Result<ReleaseAction, ActionState> {
        let mut actions = self.lock();
        Self::expire(&mut actions, &now);
        let Some(action) = actions.get_mut(id) else {
            return Err(ActionState::Failed);
        };
        if action.state != ActionState::Pending {
            return Err(action.state);
        }
        action.state = ActionState::Claimed;
        action.claimed_by = Some(by.to_string());
        action.claimed_at = Some(now);
        Ok(action.clone())
    }

    pub(super) fn finish(
        &self,
        id: &str,
        by: &str,
        result: ActionResult,
        now: String,
    ) -> Result<ReleaseAction, Option<ActionState>> {
        let mut actions = self.lock();
        Self::expire(&mut actions, &now);
        let Some(action) = actions.get_mut(id) else {
            return Err(None);
        };
        if action.state != ActionState::Claimed || action.claimed_by.as_deref() != Some(by) {
            return Err(Some(action.state));
        }
        action.state = if result.ok {
            ActionState::Succeeded
        } else {
            ActionState::Failed
        };
        action.finished_at = Some(now);
        action.message = result.message.map(|m| m.chars().take(500).collect());
        action.log_tail = result.log_tail.map(|tail| {
            let skip = tail.chars().count().saturating_sub(MAX_LOG_TAIL);
            tail.chars().skip(skip).collect()
        });
        Ok(action.clone())
    }
}

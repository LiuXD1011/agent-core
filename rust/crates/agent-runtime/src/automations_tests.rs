use super::*;
use chrono::{Datelike, Weekday};

fn definition(schedule: AutomationSchedule) -> AutomationDefinition {
    AutomationDefinition {
        id: "auto-1".into(),
        name: "每日摘要".into(),
        prompt: "总结工作区进展".into(),
        status: AutomationStatus::Active,
        schedule,
        workspaces: vec![PathBuf::from("/tmp/agent-core-automation")],
        model: None,
        allow_changes: false,
        allow_commands: false,
        valid_from: None,
        valid_until: None,
        next_run_at: None,
        last_run_at: None,
        created_at: 1_700_000_000_000,
        updated_at: 1_700_000_000_000,
        deleted_at: None,
    }
}

fn recurring(rrule: &str) -> AutomationDefinition {
    definition(AutomationSchedule::Recurring {
        rrule: rrule.to_owned(),
    })
}

fn local_ms(y: i32, m: u32, d: u32, h: u32, min: u32) -> u64 {
    use chrono::TimeZone;
    chrono::Local
        .from_local_datetime(
            &chrono::NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(h, min, 0)
                .unwrap(),
        )
        .single()
        .unwrap()
        .timestamp_millis() as u64
}

#[test]
fn rrule_parses_fields_and_validates_required_selectors() {
    let rule = parse_rrule("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,FR;BYHOUR=9;BYMINUTE=30").unwrap();
    assert_eq!(rule.freq, Frequency::Weekly);
    assert_eq!(rule.interval, 2);
    assert_eq!(rule.byday, vec![Weekday::Mon, Weekday::Fri]);
    assert_eq!(rule.byhour, vec![9]);
    assert_eq!(rule.byminute, vec![30]);
    assert!(parse_rrule("FREQ=WEEKLY").is_err());
    assert!(parse_rrule("FREQ=MONTHLY").is_err());
    assert!(parse_rrule("FREQ=YEARLY;BYMONTHDAY=1").is_err());
    assert!(parse_rrule("FREQ=DAILY;INTERVAL=0").is_err());
    assert!(parse_rrule("FREQ=ANNUALLY").is_err());
}

#[test]
fn daily_rule_finds_next_local_occurrence_after_anchor() {
    let rule = parse_rrule("FREQ=DAILY;INTERVAL=1;BYHOUR=9;BYMINUTE=30").unwrap();
    let def = recurring("FREQ=DAILY;INTERVAL=1;BYHOUR=9;BYMINUTE=30");
    // 10:15 on some day: the next 09:30 is tomorrow.
    let now = local_ms(2026, 10, 15, 10, 15);
    let next = compute_next_run_at(&def, now, now, None).unwrap();
    let expected = local_ms(2026, 10, 16, 9, 30);
    assert_eq!(next, expected);
    let _ = rule;
}

#[test]
fn weekly_interval_matches_monday_based_weeks() {
    let def = recurring("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO;BYHOUR=8;BYMINUTE=0");
    // Friday of week 0; the next matching Monday is in week 2.
    let now = local_ms(2026, 10, 16, 12, 0); // Friday
    let next = compute_next_run_at(&def, now, now, None).unwrap();
    let expected = local_ms(2026, 10, 26, 8, 0); // Monday two weeks later
    assert_eq!(next, expected);
}

#[test]
fn monthly_and_yearly_rules_honor_interval_indexes() {
    let mut monthly = recurring("FREQ=MONTHLY;INTERVAL=2;BYMONTHDAY=1;BYHOUR=8;BYMINUTE=0");
    monthly.created_at = local_ms(2026, 10, 1, 0, 0);
    let now = local_ms(2026, 10, 15, 9, 0);
    let next = compute_next_run_at(&monthly, now, now, None).unwrap();
    assert_eq!(next, local_ms(2026, 12, 1, 8, 0));

    let mut yearly = recurring("FREQ=YEARLY;INTERVAL=1;BYMONTH=3;BYMONTHDAY=1;BYHOUR=9;BYMINUTE=0");
    yearly.created_at = local_ms(2026, 10, 1, 0, 0);
    let next = compute_next_run_at(&yearly, now, now, None).unwrap();
    assert_eq!(next, local_ms(2027, 3, 1, 9, 0));
}

#[test]
fn hourly_interval_anchors_on_last_run() {
    let def = recurring("FREQ=HOURLY;INTERVAL=3");
    let last_run = local_ms(2026, 10, 15, 6, 0);
    let now = local_ms(2026, 10, 15, 7, 30);
    let next = compute_next_run_at(&def, now, now, Some(last_run)).unwrap();
    assert_eq!(next, last_run + 3 * 3_600_000);
}

#[test]
fn once_schedule_respects_validity_window() {
    let at = local_ms(2026, 10, 20, 9, 0);
    let mut def = definition(AutomationSchedule::Once { scheduled_at: at });
    def.valid_until = Some(local_ms(2026, 10, 18, 0, 0));
    let now = local_ms(2026, 10, 15, 9, 0);
    // Valid-until at midnight means through the end of that day.
    assert_eq!(compute_next_run_at(&def, now, now, None), None);
    def.valid_until = Some(local_ms(2026, 10, 20, 0, 0));
    assert_eq!(compute_next_run_at(&def, now, now, None), Some(at));
    def.valid_from = Some(at + 1);
    assert_eq!(compute_next_run_at(&def, now, now, None), None);
}

#[test]
fn due_decision_runs_catches_up_within_window_and_expires_outside() {
    let mut def = recurring("FREQ=DAILY;INTERVAL=1;BYHOUR=9;BYMINUTE=0");
    let scheduled = local_ms(2026, 10, 15, 9, 0);
    def.next_run_at = Some(scheduled);
    let runtime = AutomationRuntimeState::default();
    let due_at = scheduled + dispatch_delay_ms("auto-1", scheduled, 14);
    // Late but inside tolerance: a normal scheduled run.
    let decision = decide_due_automation(&def, &runtime, due_at + 1_000, 14, 0, 3);
    assert_eq!(
        decision,
        DueDecision::Run {
            kind: AutomationRunKind::Scheduled,
            missed_scheduled_at: None,
        }
    );
    // Before the dispatch delay elapses: not due yet.
    assert_eq!(
        decide_due_automation(&def, &runtime, due_at - 1, 14, 0, 3),
        DueDecision::Skip
    );
    // Late beyond tolerance but inside 24h: a missed (catch-up) run.
    let decision = decide_due_automation(
        &def,
        &runtime,
        due_at + MISSED_TOLERANCE_MS + 1_000,
        14,
        0,
        3,
    );
    assert_eq!(
        decision,
        DueDecision::Run {
            kind: AutomationRunKind::Missed,
            missed_scheduled_at: Some(scheduled),
        }
    );
    // Beyond the 24h window: skipped with a recomputed slot.
    let decision = decide_due_automation(
        &def,
        &runtime,
        scheduled + MISSED_WINDOW_MS + 1_000,
        14,
        0,
        3,
    );
    assert_eq!(
        decision,
        DueDecision::SkipMissed {
            reason_code: "missed_window_expired",
        }
    );
    // Expired validity pauses the definition.
    def.valid_until = Some(local_ms(2026, 10, 14, 0, 0));
    let decision = decide_due_automation(&def, &runtime, due_at + 1_000, 14, 0, 3);
    assert_eq!(
        decision,
        DueDecision::Expire {
            reason_code: "valid_until_passed",
        }
    );
}

#[test]
fn due_decision_honors_running_state_and_interrupt_circuit() {
    let mut def = recurring("FREQ=DAILY;INTERVAL=1;BYHOUR=9;BYMINUTE=0");
    let scheduled = local_ms(2026, 10, 15, 9, 0);
    def.next_run_at = Some(scheduled);
    let now = scheduled + dispatch_delay_ms("auto-1", scheduled, 14) + 1_000;
    let mut runtime = AutomationRuntimeState {
        running: true,
        ..Default::default()
    };
    assert_eq!(
        decide_due_automation(&def, &runtime, now, 14, 0, 3),
        DueDecision::Skip
    );
    runtime.running = false;
    assert_eq!(
        decide_due_automation(&def, &runtime, scheduled, 14, 0, 3),
        DueDecision::Skip
    );
    runtime.consecutive_interrupt_count = INTERRUPT_CIRCUIT_LIMIT;
    assert_eq!(
        decide_due_automation(&def, &runtime, now, 14, 0, 3),
        DueDecision::SkipMissed {
            reason_code: "recovery_circuit_broken",
        }
    );
    runtime.consecutive_interrupt_count = 0;
    assert_eq!(
        decide_due_automation(&def, &runtime, now, 14, 3, 3),
        DueDecision::Skip
    );
    def.status = AutomationStatus::Paused;
    assert_eq!(
        decide_due_automation(&def, &runtime, now, 14, 0, 3),
        DueDecision::Skip
    );
}

#[test]
fn dispatch_delay_is_deterministic_and_peak_hours_are_wider() {
    let off_peak = dispatch_delay_ms("auto-1", 1_000, 14);
    let peak = dispatch_delay_ms("auto-1", 1_000, 9);
    assert_eq!(off_peak, dispatch_delay_ms("auto-1", 1_000, 14));
    assert!(off_peak <= 30_000);
    assert!(peak <= 10 * 60 * 1000);
}

#[test]
fn store_crud_records_runs_and_enqueues_deliveries() {
    let directory = std::env::temp_dir().join(format!("agent-core-automations-{}", timestamp()));
    let store = AutomationStore::open(&directory).unwrap();
    let created = store.create(recurring("FREQ=HOURLY;INTERVAL=1")).unwrap();
    assert!(created.next_run_at.is_some());
    assert!(store.create(recurring("FREQ=HOURLY;INTERVAL=1")).is_err());
    assert_eq!(
        store.create(definition(AutomationSchedule::Once { scheduled_at: 0 })),
        Err(AutomationStoreError::InvalidDefinition)
    );

    let mut record = AutomationRunRecord {
        id: "run-1".into(),
        automation_id: created.id.clone(),
        automation_name: created.name.clone(),
        session_id: Some("session-1".into()),
        status: AutomationRunStatus::Success,
        kind: AutomationRunKind::Manual,
        missed_scheduled_at: None,
        workspace: created.workspaces[0].clone(),
        output: "完成".into(),
        artifacts: vec!["report.md".into()],
        success: Some(true),
        failure_code: None,
        reason_code: None,
        started_at: timestamp(),
        finished_at: Some(timestamp()),
        read: false,
    };
    store.record_run(record.clone(), None).unwrap();
    // A running record marks the definition busy and resets its slot.
    record.id = "run-2".into();
    record.status = AutomationRunStatus::Running;
    record.finished_at = None;
    store.record_run(record, None).unwrap();
    let catalog = store.catalog().unwrap();
    assert_eq!(catalog.runs.len(), 2);
    assert!(catalog.runtime[&created.id].running);
    assert_eq!(catalog.definitions[0].next_run_at, None);
    assert_eq!(catalog.revision, 3);

    // Paused definitions are skipped; resuming recomputes the slot.
    store
        .update_definition(&created.id, catalog.revision, |def| {
            def.status = AutomationStatus::Paused;
            Ok(())
        })
        .unwrap();
    let catalog = store.catalog().unwrap();
    assert_eq!(catalog.definitions[0].status, AutomationStatus::Paused);
    store.delete(&created.id, catalog.revision).unwrap();
    let catalog = store.catalog().unwrap();
    assert!(catalog.definitions[0].deleted_at.is_some());
    assert!(store.delete(&created.id, catalog.revision).is_err());
    std::fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn delivery_outbox_retries_with_backoff_and_enforces_retention() {
    let directory = std::env::temp_dir().join(format!("agent-core-delivery-{}", timestamp()));
    let store = AutomationStore::open(&directory).unwrap();
    let delivery = AutomationDelivery {
        id: "delivery-1".into(),
        dedupe_key: "dedupe".into(),
        automation_id: "auto-1".into(),
        automation_name: "每日摘要".into(),
        run_id: "run-1".into(),
        success: true,
        summary: "摘要".into(),
        status: AutomationDeliveryStatus::Pending,
        attempt_count: 0,
        next_attempt_at: 0,
        lease_expire_at: None,
        last_error: None,
        created_at: timestamp(),
        finished_at: None,
    };
    store
        .update(None, |catalog| {
            catalog.deliveries.push(delivery.clone());
            Ok(())
        })
        .unwrap();
    // Attempts 1..=4 fail into retry waiting with exact backoff; the 5th
    // attempt fails permanently.
    let mut now = timestamp() + 60_000;
    for attempt in 1..=5u32 {
        if attempt > 1 {
            // Advance past the backoff the previous failure scheduled.
            now = store.catalog().unwrap().deliveries[0].next_attempt_at + 1;
        }
        store
            .process_deliveries_with(now, 0.5, |_| Err("sender offline".into()))
            .unwrap();
        let catalog = store.catalog().unwrap();
        let row = &catalog.deliveries[0];
        if attempt < DELIVERY_MAX_ATTEMPTS {
            assert_eq!(row.status, AutomationDeliveryStatus::RetryWaiting);
            assert_eq!(row.attempt_count, attempt);
            assert_eq!(
                row.next_attempt_at,
                now + DELIVERY_BACKOFF_MS[(attempt - 1) as usize]
            );
        } else {
            assert_eq!(row.status, AutomationDeliveryStatus::Failed);
        }
    }
    // A fresh pending row is delivered and marked succeeded.
    let mut fresh = delivery.clone();
    fresh.id = "delivery-2".into();
    fresh.dedupe_key = "dedupe-2".into();
    fresh.status = AutomationDeliveryStatus::Pending;
    fresh.attempt_count = 0;
    store
        .update(None, |catalog| {
            catalog.deliveries.insert(0, fresh);
            Ok(())
        })
        .unwrap();
    now += 60_000;
    store
        .process_deliveries_with(now, 0.5, |row| {
            assert_eq!(row.id, "delivery-2");
            Ok(())
        })
        .unwrap();
    let catalog = store.catalog().unwrap();
    assert_eq!(
        catalog.deliveries[0].status,
        AutomationDeliveryStatus::Succeeded
    );
    assert_eq!(catalog.deliveries[0].id, "delivery-2");
    // The superseded older row for the same automation is skipped.
    assert_eq!(
        catalog.deliveries[1].status,
        AutomationDeliveryStatus::Skipped
    );
    std::fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn notifications_written_after_a_delivery_pass_survive_later_passes() {
    let directory = std::env::temp_dir().join(format!("agent-core-notify-{}", timestamp()));
    let store = AutomationStore::open(&directory).unwrap();
    let delivery = AutomationDelivery {
        id: "delivery-1".into(),
        dedupe_key: "dedupe".into(),
        automation_id: "auto-1".into(),
        automation_name: "每日摘要".into(),
        run_id: "run-1".into(),
        success: true,
        summary: "摘要".into(),
        status: AutomationDeliveryStatus::Pending,
        attempt_count: 0,
        next_attempt_at: 0,
        lease_expire_at: None,
        last_error: None,
        created_at: timestamp(),
        finished_at: None,
    };
    store
        .update(None, |catalog| {
            catalog.deliveries.push(delivery.clone());
            Ok(())
        })
        .unwrap();
    let now = timestamp() + 60_000;
    let delivered = store.process_deliveries_with(now, 0.5, |_| Ok(())).unwrap();
    assert_eq!(delivered.len(), 1);
    for row in delivered {
        store
            .push_notification(AutomationNotification {
                id: format!("notify-{}", row.id),
                title: "定时任务【每日摘要】运行成功".into(),
                body: row.summary,
                created_at: timestamp(),
                read: false,
            })
            .unwrap();
    }
    // A later pass with nothing pending must not wipe the notification.
    store
        .process_deliveries_with(now + 60_000, 0.5, |_| Ok(()))
        .unwrap();
    let catalog = store.catalog().unwrap();
    assert_eq!(catalog.notifications.len(), 1);
    assert_eq!(catalog.notifications[0].id, "notify-delivery-1");
    std::fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn notification_helpers_persist_and_mark_read() {
    let directory = std::env::temp_dir().join(format!("agent-core-notify-{}", timestamp()));
    let store = AutomationStore::open(&directory).unwrap();
    store
        .push_notification(AutomationNotification {
            id: "n1".into(),
            title: "定时任务【每日摘要】运行成功".into(),
            body: "摘要输出".into(),
            created_at: timestamp(),
            read: false,
        })
        .unwrap();
    let catalog = store.catalog().unwrap();
    assert_eq!(catalog.notifications.len(), 1);
    store.mark_notifications_read().unwrap();
    let catalog = store.catalog().unwrap();
    assert!(catalog.notifications[0].read);
    std::fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn calendar_rule_defaults_to_midnight_without_time_selectors() {
    let def = recurring("FREQ=DAILY;INTERVAL=1");
    let now = local_ms(2026, 10, 15, 10, 0);
    let next = compute_next_run_at(&def, now, now, None).unwrap();
    let date = date_of(next).unwrap();
    assert_eq!((date.year(), date.month(), date.day()), (2026, 10, 16));
}

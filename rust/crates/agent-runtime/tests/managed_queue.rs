use agent_core::agent_loop::Provider;
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Message, UserMessage,
};
use agent_runtime::runtime::{
    run_with_provider, MessageQueues, QueueError, QueueItemId, QueueLane, QueueMode, RunOptions,
};
use agent_runtime::session::Session;

#[derive(Default)]
struct Fixture {
    requests: Vec<Context>,
    fail: bool,
    cancel: bool,
}
impl Provider for Fixture {
    fn complete(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
        _: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        self.requests.push(context.clone());
        if self.cancel {
            signal.cancel();
        }
        if self.fail {
            Err("offline failure".into())
        } else {
            Ok(AssistantMessage::text("offline answer"))
        }
    }
}
fn run(
    queues: &MessageQueues,
    fixture: &mut Fixture,
    max_turns: usize,
    signal: &CancellationToken,
) -> (Session, Result<(), String>) {
    let mut session = Session::default();
    let result = run_with_provider(
        &mut session,
        Message::user("initial"),
        fixture,
        &[],
        &RunOptions {
            max_turns,
            ..Default::default()
        },
        queues,
        signal,
        &mut |_| {},
    );
    (session, result)
}
fn texts(queues: &MessageQueues) -> Vec<String> {
    queues
        .snapshot()
        .items
        .iter()
        .map(|item| item.message.text())
        .collect()
}

#[test]
fn stable_ids_edit_remove_reorder_and_snapshot_are_independent() {
    let queues = MessageQueues::default();
    let a = queues.enqueue(Message::user("a"), QueueLane::FollowUp);
    let b = queues.enqueue(Message::user("b"), QueueLane::FollowUp);
    let c = queues.enqueue(Message::user("c"), QueueLane::FollowUp);
    let mut snapshot = queues.snapshot();
    snapshot.items[0].message = Message::user("not applied");
    queues.reorder(&[c, c, QueueItemId(0), a]).unwrap();
    assert_eq!(texts(&queues), ["c", "a", "b"]);
    queues.edit(a, Message::user("edited")).unwrap();
    assert_eq!(queues.remove(b).unwrap().text(), "b");
    assert_eq!(queues.remove(b).unwrap_err(), QueueError::MissingItem);
    assert_eq!(queues.promote(b), Err(QueueError::MissingItem));
    assert_eq!(
        queues.edit(b, Message::user("missing")),
        Err(QueueError::MissingItem)
    );
    assert_eq!(texts(&queues), ["c", "edited"]);
    let other = MessageQueues::default();
    assert_ne!(other.enqueue(Message::user("a"), QueueLane::FollowUp), a);
    assert_eq!(other.remove(a).unwrap_err(), QueueError::MissingItem);
    assert_eq!(queues.snapshot().dispatched, 0);
}

#[test]
fn real_loop_respects_order_and_consumes_each_item_once() {
    let queues = MessageQueues::default();
    let a = queues.enqueue(Message::user("a"), QueueLane::FollowUp);
    let b = queues.enqueue(Message::user("b"), QueueLane::FollowUp);
    queues.reorder(&[b, a]).unwrap();
    let mut provider = Fixture::default();
    let (session, result) = run(&queues, &mut provider, 8, &CancellationToken::new());
    result.unwrap();
    assert_eq!(provider.requests.len(), 3);
    assert_eq!(
        session
            .context
            .messages
            .iter()
            .filter(|message| matches!(message, Message::User(_)))
            .map(Message::text)
            .collect::<Vec<_>>(),
        ["initial", "b", "a"]
    );
    assert!(queues.is_empty());
    assert_eq!(queues.snapshot().dispatched, 2);
    run(&queues, &mut provider, 8, &CancellationToken::new())
        .1
        .unwrap();
    assert_eq!(provider.requests.len(), 4);
}

#[test]
fn pause_retains_normal_messages_but_explicit_immediate_preserves_images() {
    let queues = MessageQueues::default();
    queues.steer(Message::user("normal steering"));
    let image = Message::User(UserMessage {
        content: vec![
            Content::text("image request"),
            Content::Image {
                data: "AQID".into(),
                mime_type: "image/png".into(),
            },
        ],
        timestamp: 123,
    });
    let image_id = queues.enqueue(image.clone(), QueueLane::FollowUp);
    queues.follow_up(Message::user("normal follow up"));
    queues.set_paused(true);
    assert!(queues.pop_next().is_none());
    let mut provider = Fixture::default();
    run(&queues, &mut provider, 8, &CancellationToken::new())
        .1
        .unwrap();
    assert_eq!(provider.requests.len(), 1);
    assert_eq!(queues.len(), 3);
    queues.promote(image_id).unwrap();
    let (session, result) = run(&queues, &mut provider, 8, &CancellationToken::new());
    result.unwrap();
    assert!(session.context.messages.contains(&image));
    assert_eq!(texts(&queues), ["normal steering", "normal follow up"]);
    assert_eq!(queues.snapshot().dispatched, 1);
    queues.set_paused(false);
    run(&queues, &mut provider, 8, &CancellationToken::new())
        .1
        .unwrap();
    assert!(queues.is_empty());
    assert_eq!(queues.snapshot().dispatched, 3);
}

#[test]
fn cancel_error_and_zero_budget_do_not_claim_success_or_drop_pending() {
    for (fail, cancel) in [(true, false), (false, true)] {
        let queues = MessageQueues::default();
        queues.follow_up(Message::user("later"));
        let mut provider = Fixture {
            fail,
            cancel,
            ..Default::default()
        };
        assert!(run(&queues, &mut provider, 8, &CancellationToken::new())
            .1
            .is_err());
        assert_eq!(texts(&queues), ["later"]);
        assert_eq!(queues.snapshot().dispatched, 0);
    }
    for (limit, cancel) in [(0, false), (8, true)] {
        let queues = MessageQueues::default();
        queues.steer(Message::user("steer"));
        queues.follow_up(Message::user("follow"));
        let signal = CancellationToken::new();
        if cancel {
            signal.cancel();
        }
        let mut provider = Fixture::default();
        assert!(run(&queues, &mut provider, limit, &signal).1.is_err());
        assert!(provider.requests.is_empty());
        assert_eq!(queues.len(), 2);
        assert_eq!(queues.snapshot().dispatched, 0);
    }
    let queues = MessageQueues::default();
    queues.follow_up(Message::user("limited"));
    let mut provider = Fixture::default();
    let _ = run(&queues, &mut provider, 1, &CancellationToken::new());
    assert_eq!(provider.requests.len(), 1);
    assert_eq!(texts(&queues), ["limited"]);
}

#[test]
fn failed_response_after_dispatch_is_not_requeued_as_if_unadmitted() {
    let queues = MessageQueues::default();
    queues.steer(Message::user("dispatched"));
    let mut provider = Fixture {
        fail: true,
        ..Default::default()
    };
    let (session, result) = run(&queues, &mut provider, 8, &CancellationToken::new());
    assert!(result.is_err());
    assert_eq!(queues.snapshot().dispatched, 1);
    assert!(queues.is_empty());
    assert_eq!(
        session
            .context
            .messages
            .iter()
            .filter(|message| message.text() == "dispatched")
            .count(),
        1
    );
}

#[test]
fn client_handoff_restore_keeps_identity_and_clear_preserves_legacy_partition() {
    let queues = MessageQueues::default();
    let a = queues.enqueue(Message::user("a"), QueueLane::FollowUp);
    queues.follow_up(Message::user("b"));
    let item = queues.take_next().unwrap();
    assert_eq!(item.id, a);
    queues.restore(item.clone()).unwrap();
    assert_eq!(queues.restore(item), Err(QueueError::DuplicateItem));
    assert_eq!(texts(&queues), ["a", "b"]);
    queues.promote(a).unwrap();
    queues.set_paused(true);
    let (steering, follow) = queues.clear();
    assert_eq!(steering[0].text(), "a");
    assert_eq!(follow[0].text(), "b");
    assert!(queues.is_empty());
    assert!(queues.snapshot().paused);
    queues.set_steering_mode(QueueMode::All);
    queues.set_follow_up_mode(QueueMode::All);
    assert_eq!(queues.modes(), (QueueMode::All, QueueMode::All));
}

#[test]
fn admission_receipts_are_exact_once_and_opt_in() {
    let queues = MessageQueues::default();
    queues.set_track_admissions(true);
    let a = queues.enqueue(Message::user("same"), QueueLane::Steering);
    let b = queues.enqueue(Message::user("same"), QueueLane::Steering);
    let mut provider = Fixture::default();
    run(&queues, &mut provider, 8, &CancellationToken::new())
        .1
        .unwrap();
    assert_eq!(queues.take_admitted_ids(), [a, b]);
    assert!(queues.take_admitted_ids().is_empty());
    queues.set_track_admissions(false);
    queues.steer(Message::user("untracked"));
    run(&queues, &mut provider, 8, &CancellationToken::new())
        .1
        .unwrap();
    assert!(queues.take_admitted_ids().is_empty());
}

#[test]
fn panic_after_drain_before_admission_restores_same_item() {
    let queues = MessageQueues::default();
    let id = queues.enqueue(Message::user("panic boundary"), QueueLane::Steering);
    let mut session = Session::default();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_with_provider(
            &mut session,
            Message::user("initial"),
            &mut Fixture::default(),
            &[],
            &RunOptions::default(),
            &queues,
            &CancellationToken::new(),
            &mut |event| {
                if let agent_core::types::AgentEvent::MessageStart { message } = event {
                    if message.text() == "panic boundary" {
                        panic!("offline emit fixture");
                    }
                }
            },
        )
    }));
    assert!(outcome.is_err());
    assert_eq!(queues.snapshot().items[0].id, id);
    assert_eq!(texts(&queues), ["panic boundary"]);
}

use agent_core::agent_loop::Provider;
use agent_core::types::{AssistantMessage, CancellationToken, Context, Message};
use agent_runtime::runtime::{run_with_provider, MessageQueues, QueueMode, RunOptions};
use agent_runtime::session::{Session, SessionStore};
use std::fs;

struct Fixture {
    prompts: Vec<String>,
    fail: bool,
}
impl Provider for Fixture {
    fn complete(
        &mut self,
        context: &Context,
        _signal: &CancellationToken,
        on_update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        self.prompts.push(
            context
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::User(_)))
                .unwrap()
                .text(),
        );
        if self.fail {
            on_update(AssistantMessage::text("partial"));
            return Err("offline failure".into());
        }
        Ok(AssistantMessage::text("offline answer"))
    }
}

#[test]
fn runtime_retains_failed_partial_transcript_and_restores_it() {
    let mut session = Session::default();
    let path = std::env::temp_dir().join(format!("agent-runtime-{}", session.id));
    let store = SessionStore::new(&path);
    let error = run_with_provider(
        &mut session,
        Message::user("prompt"),
        &mut Fixture {
            prompts: vec![],
            fail: true,
        },
        &[],
        &RunOptions::default(),
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(error, "offline failure");
    assert_eq!(session.context.messages.len(), 2);
    assert_eq!(session.context.messages[1].text(), "partial");
    store.save(&session).unwrap();
    let restored = store.list().unwrap().pop().unwrap();
    assert_eq!(restored.context, session.context);
    fs::remove_file(path.join(format!("{}.json", session.id))).unwrap();
    fs::remove_dir(path).unwrap();
}

#[test]
fn queues_can_be_shared_with_a_running_provider_and_preserve_priority() {
    let queues = MessageQueues::default();
    queues.set_steering_mode(QueueMode::OneAtATime);
    queues.steer(Message::user("steer-1"));
    queues.steer(Message::user("steer-2"));
    queues.follow_up(Message::user("follow-up"));
    let mut fixture = Fixture {
        prompts: vec![],
        fail: false,
    };
    let mut session = Session::default();
    run_with_provider(
        &mut session,
        Message::user("initial"),
        &mut fixture,
        &[],
        &RunOptions::default(),
        &queues,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(fixture.prompts, vec!["steer-1", "steer-2", "follow-up"]);
    assert!(queues.is_empty());
    assert_eq!(
        session
            .context
            .messages
            .iter()
            .filter(|m| matches!(m, Message::User(_)))
            .count(),
        4
    );
}

#[test]
fn request_suffix_is_ephemeral_and_not_repeated_in_saved_context() {
    struct Verify;
    impl Provider for Verify {
        fn complete(
            &mut self,
            context: &Context,
            _: &CancellationToken,
            _: &mut dyn FnMut(AssistantMessage),
        ) -> Result<AssistantMessage, String> {
            assert_eq!(context.system_prompt, "base\n\ninstructions");
            Ok(AssistantMessage::text("done"))
        }
    }
    let mut session = Session::default();
    session.context.system_prompt = "base".into();
    let options = RunOptions {
        system_prompt_suffix: "instructions".into(),
        ..Default::default()
    };
    for _ in 0..2 {
        run_with_provider(
            &mut session,
            Message::user("test"),
            &mut Verify,
            &[],
            &options,
            &MessageQueues::default(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    }
    assert_eq!(session.context.system_prompt, "base");
    assert_eq!(session.context.messages.len(), 4);
}

#[test]
fn turn_limit_keeps_completed_messages_for_recovery() {
    let queues = MessageQueues::default();
    queues.follow_up(Message::user("later"));
    let mut session = Session::default();
    let error = run_with_provider(
        &mut session,
        Message::user("first"),
        &mut Fixture {
            prompts: vec![],
            fail: false,
        },
        &[],
        &RunOptions {
            max_turns: 1,
            ..Default::default()
        },
        &queues,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(error.contains("limit of 1"));
    assert_eq!(session.context.messages.len(), 2);
}

#[test]
fn cancellation_records_an_aborted_message_and_clear_returns_both_queues() {
    let queues = MessageQueues::default();
    queues.steer(Message::user("a"));
    queues.follow_up(Message::user("b"));
    let (steer, follow) = queues.clear();
    assert_eq!(steer[0].text(), "a");
    assert_eq!(follow[0].text(), "b");
    assert!(queues.is_empty());
    let signal = CancellationToken::new();
    signal.cancel();
    let mut session = Session::default();
    assert_eq!(
        run_with_provider(
            &mut session,
            Message::user("prompt"),
            &mut Fixture {
                prompts: vec![],
                fail: false
            },
            &[],
            &RunOptions::default(),
            &queues,
            &signal,
            &mut |_| {}
        )
        .unwrap_err(),
        "Operation aborted"
    );
    assert_eq!(session.context.messages.len(), 2);
}

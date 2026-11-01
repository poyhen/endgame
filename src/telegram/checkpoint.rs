use grammers_session::types::UpdateState;
use grammers_session::updates::{MessageBox, State};

#[derive(Default)]
pub struct Checkpoint {
    current: Option<Primary>,
}

#[derive(Clone, Copy)]
struct Primary {
    pts: i32,
    date: i32,
    seq: i32,
}

impl Checkpoint {
    pub fn observe(&mut self, state: &State) -> Option<UpdateState> {
        let Some(MessageBox::Common { pts }) = state.message_box else {
            return None;
        };
        if self.current.is_some_and(|current| pts <= current.pts) {
            return None;
        }
        // getDifference can return many messages with the same final pts. Only
        // checkpoint a group after the next group arrives, when every earlier
        // message has been admitted to the durable inbox. Persisting the stream's
        // current state here would skip its still-buffered messages after a crash.
        let previous = self.current.replace(Primary {
            pts,
            date: state.date,
            seq: state.seq,
        });
        previous.map(|previous| UpdateState::Primary {
            pts: previous.pts,
            date: previous.date,
            seq: previous.seq,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(pts: i32) -> State {
        State {
            date: 1000,
            seq: 7,
            message_box: Some(MessageBox::Common { pts }),
        }
    }

    #[test]
    fn does_not_checkpoint_part_of_a_recovered_batch() {
        let mut checkpoint = Checkpoint::default();
        for _ in 0..100 {
            assert!(checkpoint.observe(&state(100)).is_none());
        }
        assert!(matches!(
            checkpoint.observe(&state(101)),
            Some(UpdateState::Primary { pts: 100, .. })
        ));
        assert!(checkpoint.observe(&state(101)).is_none());
    }

    #[test]
    fn late_updates_cannot_move_the_checkpoint_backwards() {
        let mut checkpoint = Checkpoint::default();
        checkpoint.observe(&state(100));
        checkpoint.observe(&state(102));
        assert!(checkpoint.observe(&state(99)).is_none());
        assert!(matches!(
            checkpoint.observe(&state(103)),
            Some(UpdateState::Primary { pts: 102, .. })
        ));
    }
}

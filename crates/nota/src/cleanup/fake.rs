//! Test stand-ins for clean-up's decider and sources: a decider that
//! answers as it's told, by heard span and term, and notes every question
//! and when it's dropped.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nota_core::decide::{
    Answer, DecideError, Decider, Decision, Decisions, Place, Probability, Question,
};
use nota_store::{DecidedBy, Pack};

use super::{Setup, Sources};

/// How the fake decider ends a question, in place of answering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trouble {
    /// As if told to stop.
    Stopped,
    /// An answer that can't be read.
    Failed,
    /// A server that can't be run.
    Unavailable,
}

/// What the fake decider shares with the test that made it.
#[derive(Debug, Default)]
pub(crate) struct Log {
    /// Each question's chunk, in order.
    pub(crate) asked: Mutex<Vec<String>>,
    /// Each question's candidates as `heard -> term`, in order.
    pub(crate) candidates: Mutex<Vec<Vec<String>>>,
    /// How many deciders were dropped.
    pub(crate) dropped: AtomicUsize,
    /// Set by `then` after the given number of questions.
    pub(crate) flag: AtomicBool,
}

/// A decider that answers yes where its script says, and no otherwise.
#[derive(Clone)]
pub(crate) struct Fake {
    /// The yeses, by heard span and term, with the probability given.
    yes: Arc<BTreeMap<(String, String), Option<Probability>>>,
    /// Trouble for the question with this number (from 0, across every
    /// decider made from this one).
    trouble: Arc<BTreeMap<usize, Trouble>>,
    /// After this many questions, `log.flag` is set.
    flag_after: Option<usize>,
    /// What it notes.
    pub(crate) log: Arc<Log>,
}

impl Fake {
    /// A decider that says yes to each `(heard, term)` in `yes`.
    pub(crate) fn saying_yes<'a>(yes: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self::with_probabilities(yes.into_iter().map(|(h, t)| (h, t, None)))
    }

    /// A decider that says yes to each `(heard, term)` in `yes`, with the
    /// probability given.
    pub(crate) fn with_probabilities<'a>(
        yes: impl IntoIterator<Item = (&'a str, &'a str, Option<f64>)>,
    ) -> Self {
        let yes = yes
            .into_iter()
            .map(|(h, t, p)| ((h.to_owned(), t.to_owned()), p.and_then(Probability::new)))
            .collect();
        Self {
            yes: Arc::new(yes),
            trouble: Arc::new(BTreeMap::new()),
            flag_after: None,
            log: Arc::new(Log::default()),
        }
    }

    /// This decider, ending question `n` (from 0) with `trouble`.
    pub(crate) fn with_trouble(mut self, n: usize, trouble: Trouble) -> Self {
        Arc::make_mut(&mut self.trouble).insert(n, trouble);
        self
    }

    /// This decider, setting its log's flag once it has answered `n`
    /// questions.
    pub(crate) const fn flagging_after(mut self, n: usize) -> Self {
        self.flag_after = Some(n);
        self
    }

    /// How many questions it has been asked.
    pub(crate) fn questions(&self) -> usize {
        self.log.asked.lock().unwrap().len()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.log.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

impl Decider for Fake {
    fn place(&self) -> Place {
        Place::Local
    }

    fn decide(
        &mut self,
        question: &Question,
        _stop: &dyn Fn() -> bool,
    ) -> Result<Decisions, DecideError> {
        let n = {
            let mut asked = self.log.asked.lock().unwrap();
            asked.push(question.current().to_owned());
            asked.len() - 1
        };
        self.log.candidates.lock().unwrap().push(
            question
                .candidates()
                .map(|(_, c)| format!("{} -> {}", c.heard(), c.term()))
                .collect(),
        );
        if self.flag_after.is_some_and(|after| n + 1 >= after) {
            self.log.flag.store(true, Ordering::SeqCst);
        }
        match self.trouble.get(&n) {
            Some(Trouble::Stopped) => return Err(DecideError::Stopped),
            Some(Trouble::Failed) => return Err(DecideError::Failed("garbled".into())),
            Some(Trouble::Unavailable) => {
                return Err(DecideError::Unavailable("no server".into()));
            }
            None => {}
        }
        let decisions = question
            .candidates()
            .map(
                |(_, c)| match self.yes.get(&(c.heard().to_owned(), c.term().to_owned())) {
                    Some(p) => Decision {
                        answer: Answer::Yes,
                        yes: *p,
                    },
                    None => Decision {
                        answer: Answer::No,
                        yes: None,
                    },
                },
            )
            .collect();
        Ok(Decisions::new(question, decisions).unwrap())
    }
}

/// What a fake source gives as the decider.
#[derive(Clone)]
pub(crate) enum Gives {
    /// No decider is set up.
    NotSetUp,
    /// The decider can't be used, for this reason.
    Unavailable(&'static str),
    /// A copy of this fake.
    Fake(Fake),
}

/// Sources that give a fake decider and the packs given.
#[derive(Clone)]
pub(crate) struct FakeSources {
    /// The decider each run gets.
    pub(crate) gives: Gives,
    /// The installed packs.
    pub(crate) packs: Vec<Pack>,
}

/// What the fake decider is recorded as.
pub(crate) fn fake_by() -> DecidedBy {
    DecidedBy {
        name: "fake".to_owned(),
        model: "as-scripted".to_owned(),
        place: Place::Local,
    }
}

impl Sources for FakeSources {
    fn decider(&self) -> Setup {
        match &self.gives {
            Gives::NotSetUp => Setup::NotSetUp,
            Gives::Unavailable(why) => Setup::Unavailable((*why).to_owned()),
            Gives::Fake(fake) => Setup::Ready(Box::new(fake.clone()), fake_by()),
        }
    }

    fn packs(&self) -> Result<Vec<Pack>, String> {
        Ok(self.packs.clone())
    }
}

/// Clean-up with no decider set up and no packs: its jobs wait.
pub(crate) fn idle() -> super::CleanUp {
    let sources = FakeSources {
        gives: Gives::NotSetUp,
        packs: Vec::new(),
    };
    super::CleanUp::new(Box::new(sources), nota_core::SampleRate::SPEECH)
}

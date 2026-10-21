//! Golden paths: sequences of fallible agents, with an auditor after each step
//! and a configurable offramp when something goes wrong.

use crate::config::Offramp;

/// Why an agent (or its auditor) rejected the work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault(pub String);

/// A single agent. State is threaded through `S`, so agents can be given
/// stateful capabilities by the journey that owns the state type.
pub trait Agent<S> {
    fn name(&self) -> &str;
    fn run(&self, state: &mut S) -> Result<(), Fault>;
}

/// Checks the output of the step it follows.
pub trait Auditor<S> {
    fn audit(&self, state: &S) -> Result<(), Fault>;
}

struct Step<S> {
    agent: Box<dyn Agent<S>>,
    auditor: Option<Box<dyn Auditor<S>>>,
}

pub struct GoldenPath<S> {
    pub name: String,
    steps: Vec<Step<S>>,
}

/// How a run ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    /// The agent fell off the path; the offramp says what to do about it.
    FellOff {
        step: String,
        fault: Fault,
        offramp: Offramp,
    },
}

impl<S> GoldenPath<S> {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            steps: vec![],
        }
    }

    pub fn step(mut self, agent: impl Agent<S> + 'static) -> Self {
        self.steps.push(Step {
            agent: Box::new(agent),
            auditor: None,
        });
        self
    }

    /// Add a step whose result is checked by `auditor`.
    pub fn audited_step(
        mut self,
        agent: impl Agent<S> + 'static,
        auditor: impl Auditor<S> + 'static,
    ) -> Self {
        self.steps.push(Step {
            agent: Box::new(agent),
            auditor: Some(Box::new(auditor)),
        });
        self
    }

    pub fn run(&self, state: &mut S, offramp: Offramp) -> Outcome {
        for step in &self.steps {
            let result = step
                .agent
                .run(state)
                .and_then(|()| step.auditor.as_ref().map_or(Ok(()), |a| a.audit(state)));
            if let Err(fault) = result {
                return Outcome::FellOff {
                    step: step.agent.name().into(),
                    fault,
                    offramp,
                };
            }
        }
        Outcome::Completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Push(&'static str);
    impl Agent<Vec<&'static str>> for Push {
        fn name(&self) -> &str {
            self.0
        }
        fn run(&self, s: &mut Vec<&'static str>) -> Result<(), Fault> {
            s.push(self.0);
            Ok(())
        }
    }

    struct Boom;
    impl Agent<Vec<&'static str>> for Boom {
        fn name(&self) -> &str {
            "boom"
        }
        fn run(&self, _: &mut Vec<&'static str>) -> Result<(), Fault> {
            Err(Fault("nope".into()))
        }
    }

    struct MaxLen(usize);
    impl Auditor<Vec<&'static str>> for MaxLen {
        fn audit(&self, s: &Vec<&'static str>) -> Result<(), Fault> {
            if s.len() <= self.0 {
                Ok(())
            } else {
                Err(Fault("too long".into()))
            }
        }
    }

    #[test]
    fn completes() {
        let p = GoldenPath::new("p")
            .step(Push("a"))
            .audited_step(Push("b"), MaxLen(2));
        let mut s = vec![];
        assert_eq!(p.run(&mut s, Offramp::Noop), Outcome::Completed);
        assert_eq!(s, ["a", "b"]);
    }

    #[test]
    fn agent_failure_falls_off() {
        let p = GoldenPath::new("p")
            .step(Push("a"))
            .step(Boom)
            .step(Push("c"));
        let mut s = vec![];
        assert_eq!(
            p.run(&mut s, Offramp::Issue),
            Outcome::FellOff {
                step: "boom".into(),
                fault: Fault("nope".into()),
                offramp: Offramp::Issue
            }
        );
        assert_eq!(s, ["a"]);
    }

    #[test]
    fn auditor_failure_falls_off() {
        let p = GoldenPath::new("p").audited_step(Push("a"), MaxLen(0));
        let mut s = vec![];
        assert!(matches!(
            p.run(&mut s, Offramp::Escalate),
            Outcome::FellOff {
                offramp: Offramp::Escalate,
                ..
            }
        ));
    }
}

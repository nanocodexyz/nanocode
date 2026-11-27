use crate::app::{App, AppEvent};
use nanocode::{Agent, NanocodeError, SessionTitle};

pub(crate) enum Naming {
    Reading {
        session: String,
        prompt: Option<String>,
    },
    Untitled {
        session: String,
    },
    Generating {
        session: String,
    },
    Titled {
        session: String,
        title: String,
    },
    Unsupported {
        session: String,
    },
}

pub(crate) enum TitleUpdate {
    Read(Result<Option<String>, NanocodeError>),
    Generated(Result<Option<String>, NanocodeError>),
    Renamed(Result<Option<String>, String>),
}

impl Naming {
    fn session(&self) -> &str {
        match self {
            Self::Reading { session, .. }
            | Self::Untitled { session }
            | Self::Generating { session }
            | Self::Titled { session, .. }
            | Self::Unsupported { session } => session,
        }
    }
}

impl App {
    pub(crate) fn title(&self) -> Option<&str> {
        match &self.naming {
            Some(Naming::Titled { title, .. }) => Some(title),
            _ => None,
        }
    }

    pub(crate) fn read_title(&mut self, agent: &Agent) {
        let session = agent.session_id().to_owned();
        if self
            .naming
            .as_ref()
            .is_some_and(|naming| naming.session() == session)
        {
            return;
        }
        self.naming = Some(Naming::Reading {
            session: session.clone(),
            prompt: None,
        });
        let agent = agent.clone();
        let tx = self.tx.clone();
        nanocode_platform::spawn_task(async move {
            let result = agent.title().await;
            drop(tx.send(AppEvent::Title {
                session,
                update: TitleUpdate::Read(result),
            }));
        });
    }

    pub(crate) fn name_session(&mut self, agent: &Agent, prompt: &str) {
        let session = agent.session_id();
        match self.naming.as_mut() {
            Some(Naming::Reading {
                session: reading,
                prompt: pending,
            }) if reading == session => {
                pending.get_or_insert_with(|| prompt.to_owned());
            }
            Some(Naming::Untitled { session: untitled }) if untitled == session => {
                self.generate_title(agent.clone(), prompt.to_owned());
            }
            _ => {}
        }
    }

    fn generate_title(&mut self, agent: Agent, prompt: String) {
        let session = agent.session_id().to_owned();
        self.naming = Some(Naming::Generating {
            session: session.clone(),
        });
        let tx = self.tx.clone();
        nanocode_platform::spawn_task(async move {
            let result = match agent.generate_title(prompt).await {
                Ok(Some(title)) => agent.set_title(SessionTitle::Generated(title)).await,
                outcome => outcome,
            };
            drop(tx.send(AppEvent::Title {
                session,
                update: TitleUpdate::Generated(result),
            }));
        });
    }

    pub(crate) fn rename(&mut self, title: &str) {
        let Some(agent) = self.agent.clone() else {
            self.flash("the agent is still starting");
            return;
        };
        let title = title.trim();
        if title.is_empty() {
            self.flash("usage: /rename <title>");
            return;
        }
        let session = agent.session_id().to_owned();
        let title = title.to_owned();
        let tx = self.tx.clone();
        nanocode_platform::spawn_task(async move {
            let result = agent
                .set_title(SessionTitle::Chosen(title))
                .await
                .map_err(|error| error.to_string());
            drop(tx.send(AppEvent::Title {
                session,
                update: TitleUpdate::Renamed(result),
            }));
        });
    }

    pub(crate) fn title_update(&mut self, session: String, update: TitleUpdate) {
        if self
            .naming
            .as_ref()
            .is_none_or(|naming| naming.session() != session)
        {
            return;
        }
        let Some(previous) = self.naming.take() else {
            return;
        };
        let pending = match &previous {
            Naming::Reading { prompt, .. } => prompt.clone(),
            _ => None,
        };
        self.naming = Some(match update {
            TitleUpdate::Read(Ok(Some(title)))
            | TitleUpdate::Generated(Ok(Some(title)))
            | TitleUpdate::Renamed(Ok(Some(title))) => Naming::Titled { session, title },
            TitleUpdate::Read(Ok(None)) | TitleUpdate::Generated(Ok(None)) => {
                Naming::Untitled { session }
            }
            TitleUpdate::Renamed(Ok(None)) => {
                self.flash("the session was not renamed");
                previous
            }
            TitleUpdate::Read(Err(NanocodeError::UnsupportedCapability { .. }))
            | TitleUpdate::Generated(Err(NanocodeError::UnsupportedCapability { .. })) => {
                Naming::Unsupported { session }
            }
            TitleUpdate::Read(Err(error)) => {
                self.flash(format!("session title: {error}"));
                Naming::Unsupported { session }
            }
            TitleUpdate::Renamed(Err(error)) => {
                self.flash(format!("session title: {error}"));
                previous
            }
            TitleUpdate::Generated(Err(error)) => {
                nanocode_trace::warn!(target: "nanocode", %error, "session title generation failed");
                Naming::Untitled { session }
            }
        });
        if let (Some(prompt), Some(agent)) = (pending, self.agent.clone())
            && agent.session_id() == self.naming.as_ref().map_or("", Naming::session)
        {
            self.name_session(&agent, &prompt);
        }
    }
}

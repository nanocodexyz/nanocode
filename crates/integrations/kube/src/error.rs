use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Config {
        problem: String,
    },
    Transport {
        detail: String,
        retryable: bool,
    },
    Status {
        code: u16,
        reason: String,
        message: String,
    },
    Conflict {
        message: String,
    },
    Gone {
        message: String,
    },
    TooLarge {
        bound: &'static str,
        limit: u64,
    },
    Full {
        bound: &'static str,
        limit: u64,
    },
    Malformed {
        problem: String,
    },
}

impl Error {
    pub fn config(problem: impl Into<String>) -> Self {
        Self::Config {
            problem: problem.into(),
        }
    }

    pub fn malformed(problem: impl Into<String>) -> Self {
        Self::Malformed {
            problem: problem.into(),
        }
    }

    pub const fn retryable(&self) -> bool {
        match self {
            Self::Transport { retryable, .. } => *retryable,
            Self::Status { code, .. } => *code == 429 || *code >= 500,
            _ => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config { problem } => write!(formatter, "kube config: {problem}"),
            Self::Transport { detail, .. } => write!(formatter, "kube transport: {detail}"),
            Self::Status {
                code,
                reason,
                message,
            } => write!(formatter, "kube status {code} {reason}: {message}"),
            Self::Conflict { message } => write!(formatter, "kube conflict: {message}"),
            Self::Gone { message } => write!(formatter, "kube gone: {message}"),
            Self::TooLarge { bound, limit } => write!(formatter, "kube {bound} {limit} exceeded"),
            Self::Full { bound, limit } => write!(formatter, "kube {bound} {limit} full"),
            Self::Malformed { problem } => write!(formatter, "kube malformed: {problem}"),
        }
    }
}

impl std::error::Error for Error {}

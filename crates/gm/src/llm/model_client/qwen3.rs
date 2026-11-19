use linkme::distributed_slice;

use super::{AnswerTokens, CLIENTS, Inference, Mode, ModelClient, Registration, Task};

pub(crate) const QWEN3_MODEL: &str = "mlx-community/Qwen3-8B-4bit";

#[distributed_slice(CLIENTS)]
static REGISTRATION: Registration = Registration {
    name: "Qwen3",
    models: &[QWEN3_MODEL],
    client: &Qwen3Client,
    fallback: true,
};

struct Qwen3Client;

const NO_THINK_TAG: &str = "/no_think";

impl ModelClient for Qwen3Client {
    fn easy(&self, task: Task) -> Inference {
        Inference {
            answer: answer_tokens(task),
            mode: Mode::Direct {
                system_suffix: NO_THINK_TAG,
            },
        }
    }

    fn medium(&self, task: Task) -> Inference {
        self.easy(task)
    }

    fn hard(&self, task: Task) -> Inference {
        Inference {
            answer: answer_tokens(task),
            mode: Mode::Thinking {
                reasoning_tokens: 1024,
                decode_answer: std::convert::identity,
                fallback_tag: Some(NO_THINK_TAG),
                language_check: false,
            },
        }
    }
}

fn answer_tokens(task: Task) -> AnswerTokens {
    match task {
        Task::Router => AnswerTokens::Fixed(60),
        Task::Triage => AnswerTokens::Fixed(80),
        Task::DigestLine => AnswerTokens::Fixed(150),
        Task::DigestJobs => AnswerTokens::Fixed(600),
        Task::Summary => AnswerTokens::ConfiguredSummary,
        Task::Translate => AnswerTokens::Fixed(2048),
        Task::Analyze => AnswerTokens::Fixed(1024),
        Task::Reply => AnswerTokens::Fixed(512),
    }
}

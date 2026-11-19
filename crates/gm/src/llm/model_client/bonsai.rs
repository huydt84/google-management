use linkme::distributed_slice;

use super::{AnswerTokens, CLIENTS, Inference, Mode, ModelClient, Registration, Task};

#[distributed_slice(CLIENTS)]
static STOCK: Registration = Registration {
    name: "Bonsai",
    models: &["prism-ml/Bonsai-27B-mlx-1bit"],
    client: &BonsaiClient { direct: false },
    fallback: false,
};

#[distributed_slice(CLIENTS)]
static DIRECT: Registration = Registration {
    name: "Bonsai direct",
    models: &["Bonsai-27B-mlx-1bit-direct"],
    client: &BonsaiClient { direct: true },
    fallback: false,
};

struct BonsaiClient {
    direct: bool,
}

impl ModelClient for BonsaiClient {
    fn easy(&self, task: Task) -> Inference {
        self.plan(task)
    }

    fn medium(&self, task: Task) -> Inference {
        self.plan(task)
    }

    fn hard(&self, task: Task) -> Inference {
        self.plan(task)
    }
}

impl BonsaiClient {
    fn plan(&self, task: Task) -> Inference {
        let mode = if self.direct {
            Mode::Direct {
                system_suffix: if task == Task::Triage {
                    "Choose notify when the email asks the owner to do something or mentions a deadline. A meeting invitation that requests materials or an RSVP is notify."
                } else {
                    ""
                },
            }
        } else {
            Mode::Thinking {
                reasoning_tokens: match task {
                    Task::Router
                    | Task::Triage
                    | Task::DigestLine
                    | Task::DigestJobs
                    | Task::Summary
                    | Task::Translate => 1536,
                    Task::Analyze | Task::Reply => 2048,
                },
                decode_answer,
                fallback_tag: None,
                language_check: true,
            }
        };
        Inference {
            answer: answer_tokens(task),
            mode,
        }
    }
}

fn answer_tokens(task: Task) -> AnswerTokens {
    AnswerTokens::Fixed(match task {
        Task::Router => 80,
        Task::Triage => 112,
        Task::DigestLine => 192,
        Task::DigestJobs => 704,
        Task::Summary => 512,
        Task::Translate => 2304,
        Task::Analyze => 1280,
        Task::Reply => 640,
    })
}

/// The stock template opens `<think>` in the prompt. The answer starts after the closing tag.
fn decode_answer(text: String) -> String {
    text.split_once("</think>")
        .map_or("", |(_, answer)| answer.trim())
        .to_owned()
}

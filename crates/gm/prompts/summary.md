Task: summarize one email for a phone notification.

Return exactly these keys:
{"summary": string,             // 2-4 short sentences in the output language
 "importance": "low" | "medium" | "high",  // how much the owner needs to see or act on it soon
 "category": string,            // personal, work, finance, billing, shopping, travel, security, newsletter, jobs, other
 "suggested_actions": [string], // at most 3 short options in the output language, e.g. translate, reply
 "language": string}            // the language the email is written in

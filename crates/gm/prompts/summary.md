Task: summarize one email for a phone notification.

Return exactly these keys:
{"summary": string,             // 2-4 short sentences in the output language
 "importance": "low" | "medium" | "high",  // how much the owner needs to see or act on it soon
 "category": string,            // personal, work, finance, billing, shopping, travel, security, newsletter, jobs, other
 "suggested_actions": [{"text": string, "link": number}], // at most 3 short next steps in the output language, e.g. translate, reply
 "language": string}            // the language the email is written in

"link" is the number of an entry in <links> that the step opens, for example the page to check
or the run to view. Leave "link" out when there is no <links> block or no entry fits the step.

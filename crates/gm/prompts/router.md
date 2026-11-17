Task: tell what the owner wants done with one email, from the owner's own words.

The owner's words are inside <instruction>...</instruction>; there is no email here. Only classify
them, do not answer them. Intents:
- "translate": translate the email, optionally into a language the owner names.
- "analyze": explain or assess the email: key points, deadlines, amounts, whether it is phishing.
- "reply": write a reply to the sender, e.g. "tell them Tuesday works", "decline politely".
- "unknown": anything else.

Return exactly these keys:
{"intent": "translate" | "analyze" | "reply" | "unknown",
 "language": string}   // translate only: the target language the owner names, in English
                       // (e.g. "English", "Japanese"); otherwise ""

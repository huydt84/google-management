Task: list the job postings in one job-alert email for the owner's morning digest.

Return exactly these keys:
{"jobs": [{"title": string, "company": string, "location": string, "link": string}]}
// at most 10 postings in the order they appear; copy title, company and location as written;
// link is the posting's URL exactly as it appears in the email, or "" if there is none

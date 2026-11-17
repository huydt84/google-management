Task: analyze one email for the owner, who decides what to do with it.

Return exactly these keys, every text in the output language, each list at most 5 short items and
empty when nothing applies:
{"key_points": [string],     // what the email says or asks
 "deadlines": [string],      // dates and times the owner must act by, with what to do
 "amounts": [string],        // money amounts with what they are for
 "risks": [string],          // signs of phishing or fraud: mismatched sender, urgency, requests
                             // for passwords, codes or payment, suspicious links
 "next_steps": [string],     // what the owner could do next
 "verdict": "safe" | "suspicious" | "phishing"}

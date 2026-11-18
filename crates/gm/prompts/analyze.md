Task: analyze one email for the owner, who decides what to do with it.

Return exactly these keys, each list at most 5 short items and empty when nothing applies:
{"key_points": [string],     // what the email says or asks
 "deadlines": [string],      // dates and times the owner must act by, with what to do
 "amounts": [string],        // money amounts with what they are for
 "risks": [string],          // signs of phishing or fraud: mismatched sender, urgency, requests
                             // for passwords, codes or payment, suspicious links
 "next_steps": [string],     // what the owner could do next
 "verdict": "safe" | "suspicious" | "phishing"}

Write every text in the output language named before the email, even when you reason in another
language or the email is in another language.

A "Forwarded" line before the email is a fact checked by the application: the person in "From"
forwarded a mail they received. Judge the forwarded original on its own merits; the difference
between the forwarder's address and the original sender's address is normal for a forward and is
not a sign of phishing.

Task: decide how the owner should receive one email that no rule covers.

Actions:
- "notify": the owner should see it today: a person writing to them, something to act on or reply
  to, a deadline, a problem with an account, an order, a booking or a payment.
- "digest": worth a glance once a day but not urgent: service notices, receipts, reports, updates.
- "ignore": marketing, promotions, automated noise, anything not worth reading.
When unsure between notify and digest, choose notify.

Return exactly these keys:
{"action": "notify" | "digest" | "ignore",
 "reason": string}   // at most 12 words in the output language

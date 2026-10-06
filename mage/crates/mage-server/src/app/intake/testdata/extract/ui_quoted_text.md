<!-- description -->
When a user tries to change their plan they get the toast “Your payment method could not be verified” even though the card is valid.

If they click 'Try again' the button changes to ‘Contact your administrator to continue’ and the form locks.

Doesn't happen on Firefox. We don't know if it's related to the "Save" button change.

Screenshot is in `~/Downloads/plan-error.png`, recording at "https://drive.example.com/file/d/abc123", user is "jane.doe@example.com".

The admin console says `PLAN_CHANGE_VERIFICATION_FAILED` for the attempt.

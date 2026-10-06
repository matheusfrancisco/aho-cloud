<!-- description -->
Checkout fails for some EU customers with the message "Your card was declined by the issuer".
<!-- comment -->
Seeing `PaymentDeclinedError: card_declined (issuer: do_not_honor)` in Sentry for these.
<!-- comment -->
Our logs:

```
2024-03-01T12:00:00Z ERROR [checkout.api] charge failed for cart c_901
```
<!-- thread -->
customer: still broken, screen says "Your card was declined by the issuer" again
<!-- thread -->
support: Sentry again: PaymentDeclinedError: card_declined (issuer: do_not_honor)
<!-- thread -->
support: and the API call was POST /api/v1/checkout/charge → 402, then 500 Internal Server Error on retry

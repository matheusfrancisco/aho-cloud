<!-- description -->
Mobile app shows a blank screen on the orders tab. Proxy logs:

```
10.0.4.17 - - [01/Mar/2024:10:00:01 +0000] "GET /api/v2/orders?page=2 HTTP/1.1" 500 1534 "-" "AcmeMobile/5.2.1"
10.0.4.17 - - [01/Mar/2024:10:00:02 +0000] "GET /api/v2/orders/404 HTTP/1.1" 200 88 "-" "AcmeMobile/5.2.1"
10.0.4.17 - - [01/Mar/2024:10:00:03 +0000] "POST https://api.example.com/api/v2/orders/sync HTTP/1.1" 429 41 "-" "AcmeMobile/5.2.1"
```

The web app is fine. We have about 1,500 affected users, all on version 5.2.1.
<!-- comment -->
Retried at 10:30, now the gateway answers 503 Service Unavailable.
<!-- comment -->
Plan costs $500 per month, so this is a priority customer.

<!-- description -->
Webhooks to the customer's endpoint stopped being delivered around 09:00 UTC.

```
time="2024-03-01T09:02:11Z" level=info msg="dispatching webhook" endpoint_id=ep_77
time="2024-03-01T09:02:13Z" level=error msg="webhook delivery failed: x509: certificate signed by unknown authority" endpoint_id=ep_77 attempt=1
time="2024-03-01T09:02:13Z" level=error msg="webhook delivery failed: x509: certificate signed by unknown authority" endpoint_id=ep_77 attempt=2
level=warn msg="queue depth above threshold" depth=1200
{"timestamp":"2024-03-01T09:05:00.000Z","level":"ERROR","logger":"delivery.scheduler","message":"giving up on endpoint ep_77 after 5 attempts"}
{"timestamp":"2024-03-01T09:05:01.000Z","level":"info","message":"endpoint ep_77 disabled"}
{"level":"error","status":502,"method":"POST","path":"/hooks/acme?sig=abc","msg":"upstream responded with an error"}
2024-03-01 09:06:12,881 - delivery.http - ERROR - Connection reset by peer while posting to endpoint ep_77
ERROR:root:Retry budget exhausted for tenant t_19
[2024-03-01 09:06:13] [error] signature check failed: header X-Acme-Signature missing
```

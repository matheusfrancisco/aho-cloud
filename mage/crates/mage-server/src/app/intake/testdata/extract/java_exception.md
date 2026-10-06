<!-- description -->
Customer can't re-send invoice INV-2024-0042, the API responds 500.

```
2024-03-01 10:15:42.118 ERROR 1 --- [nio-8080-exec-7] c.a.b.web.InvoiceController : Request processing failed for invoice INV-2024-0042
java.lang.IllegalStateException: Invoice INV-2024-0042 is already finalized
	at com.acme.billing.invoice.InvoiceService.resend(InvoiceService.java:88)
	at com.acme.billing.web.InvoiceController.resend(InvoiceController.java:51)
	at java.base/jdk.internal.reflect.DirectMethodHandleAccessor.invoke(DirectMethodHandleAccessor.java:103)
Caused by: org.postgresql.util.PSQLException: ERROR: duplicate key value violates unique constraint "invoices_number_key"
  Detail: Key (number)=(INV-2024-0042) already exists.
	at org.postgresql.core.v3.QueryExecutorImpl.receiveErrorResponse(QueryExecutorImpl.java:2713)
	... 42 more
```
<!-- comment -->
Exception in thread "scheduler-1" java.util.ConcurrentModificationException: null
	at java.base/java.util.ArrayList$Itr.checkForComodification(ArrayList.java:1013)

<!-- description -->
Invoice finalization crashes the billing worker since this morning's deploy (v2.14.0).

Logs from the pod `billing-worker-7d9f8c6b5-x2x9k`:

```
panic: runtime error: invalid memory address or nil pointer dereference
[signal SIGSEGV: segmentation violation code=0x1 addr=0x18 pc=0x1029c4e40]

goroutine 42 [running]:
github.com/acme/billing/internal/invoice.(*Service).Finalize(0x0, {0x102b1c2a8, 0xc000222000}, {0xc0001b6000, 0x24})
	/build/internal/invoice/service.go:118 +0x40
github.com/acme/billing/internal/worker.(*Runner).handleError(0xc000110000, {0x102b1c2a8, 0xc000222000})
	/build/internal/worker/runner.go:77 +0x1d4
created by github.com/acme/billing/internal/worker.Start in goroutine 1
	/build/internal/worker/runner.go:31 +0x88
```

Before the panic the worker logs:

```
time=2024-03-01T10:15:42.118Z level=ERROR msg="finalize invoice failed" invoice_id=inv_8f2a err="tax provider: context deadline exceeded"
time=2024-03-01T10:15:42.120Z level=INFO msg="retrying job" attempt=3
```
<!-- comment -->
Same on staging. The customer sees "We couldn't finalize your invoice" in the dashboard.

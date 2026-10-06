<!-- description -->
Exporting a report as CSV fails for workspace ws_1029 (see screenshot). Sentry shows:

    Traceback (most recent call last):
      File "/srv/app/reports/views.py", line 214, in export_csv
        rows = build_rows(report, request.user)
      File "/srv/app/reports/export.py", line 88, in build_rows
        value = row.metrics["conversion_rate"]
                ~~~~~~~~~~~^^^^^^^^^^^^^^^^^^^
    KeyError: 'conversion_rate'

The page then shows `Something went wrong while exporting your report.` and nothing is downloaded.

It also happens for the PDF export, different error though:

```
Traceback (most recent call last):
  File "/srv/app/reports/pdf.py", line 41, in render
    raise RenderTimeout(seconds)
reports.pdf.RenderTimeout: rendering took longer than 30 seconds
```
<!-- comment -->
Looks like the metric was renamed in /srv/app/reports/metrics.py last week, see https://github.com/acme/app/pull/4411

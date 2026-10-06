<!-- description -->
Hi team! Customer reports that sync is "slow sometimes" and asked us to look at it.

Relevant config is in `/etc/acme/sync.yaml` and `services/sync/config/default.toml`, docs at "https://docs.example.com/sync/troubleshooting".
They're on build `2024.03.01-rc2` and the `sync-worker` image.

The sync status page says "Last sync failed: upstream returned an unexpected response" and the CLI prints:

```
$ acme sync --verbose
fetching changes from remote... done
Error: sync aborted: remote rejected batch 17 (schema mismatch on field "created_at")
```

We already checked the 3 ports (443, 8443, 5432), the 45 min timeout and the 404.2 build of the agent; none of that is related.
<!-- thread -->
Customer just pinged again: they now see "Last sync failed: upstream returned an unexpected response" on every run.
<!-- thread -->
Also `ERROR [sync.agent] batch 17 rejected by remote` in the agent log.

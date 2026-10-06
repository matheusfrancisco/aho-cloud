<!-- description -->
**Steps**
1. Open https://app.example.com/settings/team
2. Click "Remove member" on any pending invite
3. Nothing happens

Console:

```
Uncaught (in promise) TypeError: Cannot read properties of undefined (reading 'email')
    at TeamMemberRow.handleRemove (TeamMemberRow.tsx:57:31)
    at async onClick (Button.tsx:22:5)
    at async Promise.all (index 0)
```

Network tab shows `DELETE /api/v1/teams/team_42/members/undefined` returning 404 Not Found.
<!-- comment -->
Also seeing this in the server logs:

```
{"level":"error","time":1709287200000,"msg":"member lookup failed","err":{"type":"NotFoundError","message":"NotFoundError: member undefined does not exist in team team_42"},"req":{"method":"DELETE"}}
```

# Vermon

Node version monitoring for Rotko's IBP members.

## IBP Setup

### IP Whitelist
Add to your firewall/config:
```
160.22.180.0/23
```

### Config File
Add your prometheus endpoint to the IBP config:
```json
{
  "YourMemberName": "https://your-prometheus.example.com"
}
```

## Deployment

Uses Docker on port 3000:
```bash
docker run -d --name vermon -p 3000:3000 --restart unless-stopped ghcr.io/YOUR_USERNAME/vermon:latest
```

Configure GitHub secrets: `DEPLOY_KEY`, `DEPLOY_USER`, `DEPLOY_HOST`
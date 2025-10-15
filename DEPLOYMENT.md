# Deployment

This project uses GitHub Actions to automatically build and deploy to your server.

## Required GitHub Secrets

Configure these secrets in your GitHub repository settings (`Settings` → `Secrets and variables` → `Actions`):

- **`DEPLOY_KEY`** - SSH private key for accessing your deployment server
- **`DEPLOY_USER`** - SSH username (e.g., `root` or `ubuntu`)
- **`DEPLOY_HOST`** - Server hostname or IP address (e.g., `example.com` or `192.168.1.100`)

## How It Works

1. The workflow builds a multi-arch Docker image (amd64 + arm64)
2. Pushes it to GitHub Container Registry at `ghcr.io/YOUR_USERNAME/vermon:latest`
3. SSHs into your server and pulls the new image
4. Stops the old container and starts a new one

## Manual Deployment

Trigger deployment manually from the Actions tab or push to the `master` branch (if enabled in the workflow).

## Server Requirements

- Docker installed
- Port 3000 accessible
- SSH access configured with the provided key

## Environment Variables

The container uses port 3000 by default. Override with `PORT` environment variable if needed:

```bash
docker run -d --name vermon -p 3000:3000 -e PORT=3000 --restart unless-stopped ghcr.io/YOUR_USERNAME/vermon:latest
```

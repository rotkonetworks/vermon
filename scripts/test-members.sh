#!/bin/bash
curl -s "https://raw.githubusercontent.com/ibp-network/config/refs/heads/main/members.json" | jq 'keys | .[0:3]'

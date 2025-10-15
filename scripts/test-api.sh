#!/bin/bash

# Version Monitor API Test Script
# This script tests all API endpoints

BASE_URL="http://localhost:3000"

echo "================================"
echo "Version Monitor API Test Script"
echo "================================"
echo ""

# Color codes
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Helper function to make requests
test_endpoint() {
    local method=$1
    local endpoint=$2
    local description=$3

    echo -e "${BLUE}Testing:${NC} $description"
    echo -e "${YELLOW}$method${NC} $BASE_URL$endpoint"
    echo ""

    curl -s -X $method "$BASE_URL$endpoint" | jq '.'

    echo ""
    echo "---"
    echo ""
}

# Test 1: Root endpoint
test_endpoint "GET" "/" "Root endpoint"

# Test 2: Health check
test_endpoint "GET" "/health" "Health status"

# Test 3: List all networks
test_endpoint "GET" "/api/networks" "List all networks (empty results)"

# Test 4: List all networks including empty
test_endpoint "GET" "/api/networks?include_empty=true" "List all networks (including empty)"

# Test 5: Get specific network (polkadot)
test_endpoint "GET" "/api/networks/polkadot" "Get Polkadot network info"

# Test 6: Get specific network (kusama)
test_endpoint "GET" "/api/networks/kusama" "Get Kusama network info"

# Test 7: Get specific network (asset-hub-polkadot)
test_endpoint "GET" "/api/networks/asset-hub-polkadot" "Get Asset Hub Polkadot info"

# Test 8: List all repositories
test_endpoint "GET" "/api/repos" "List all repositories"

# Test 9: Get specific repository (polkadot-sdk)
test_endpoint "GET" "/api/repos/paritytech/polkadot-sdk" "Get polkadot-sdk repository"

# Test 10: Get specific repository (encointer)
test_endpoint "GET" "/api/repos/encointer/encointer-parachain" "Get encointer-parachain repository"

# Test 11: Compare all versions
test_endpoint "GET" "/api/compare" "Compare all network versions"

# Test 12: Compare specific network (polkadot)
test_endpoint "GET" "/api/compare/polkadot" "Compare Polkadot versions"

# Test 13: Compare specific network (kusama)
test_endpoint "GET" "/api/compare/kusama" "Compare Kusama versions"

# Test 14: Compare specific network (asset-hub-polkadot)
test_endpoint "GET" "/api/compare/asset-hub-polkadot" "Compare Asset Hub Polkadot versions"

# Test 15: List all members
test_endpoint "GET" "/api/members" "List all members (providers)"

# Test 16: Get specific member (Rotko)
test_endpoint "GET" "/api/members/rotko" "Get Rotko member info"

# Test 17: Trigger manual refresh (optional - commented out to avoid rate limiting)
# echo -e "${BLUE}Testing:${NC} Trigger manual refresh"
# echo -e "${YELLOW}GET${NC} $BASE_URL/api/refresh"
# echo ""
# curl -s -X GET "$BASE_URL/api/refresh" | jq '.'
# echo ""
# echo "---"
# echo ""

# Test 18: Error handling - non-existent network
echo -e "${BLUE}Testing:${NC} Error handling - non-existent network"
echo -e "${YELLOW}GET${NC} $BASE_URL/api/networks/nonexistent"
echo ""
curl -s -w "\nHTTP Status: %{http_code}\n" -X GET "$BASE_URL/api/networks/nonexistent" | jq '.'
echo ""
echo "---"
echo ""

# Test 19: Error handling - non-existent repository
echo -e "${BLUE}Testing:${NC} Error handling - non-existent repository"
echo -e "${YELLOW}GET${NC} $BASE_URL/api/repos/nonexistent/repo"
echo ""
curl -s -w "\nHTTP Status: %{http_code}\n" -X GET "$BASE_URL/api/repos/nonexistent/repo" | jq '.'
echo ""
echo "---"
echo ""

echo -e "${GREEN}All tests completed!${NC}"
echo ""
echo "Summary of available endpoints:"
echo "  GET  /                              - API version info"
echo "  GET  /health                        - Health status"
echo "  GET  /api/networks                  - List all networks"
echo "  GET  /api/networks/{network}        - Get specific network"
echo "  GET  /api/repos                     - List all repositories"
echo "  GET  /api/repos/{owner}/{name}      - Get specific repository"
echo "  GET  /api/compare                   - Compare all versions"
echo "  GET  /api/compare/{network}         - Compare specific network"
echo "  GET  /api/members                   - List all members (providers)"
echo "  GET  /api/members/{provider}        - Get specific member info"
echo "  GET  /api/refresh                   - Trigger manual refresh"

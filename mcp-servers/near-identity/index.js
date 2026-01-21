const { Server } = require("@modelcontextprotocol/sdk/server/index.js");
const { StdioServerTransport } = require("@modelcontextprotocol/sdk/server/stdio.js");
const { CallToolRequestSchema, ListToolsRequestSchema } = require("@modelcontextprotocol/sdk/types.js");
const nearApi = require("near-api-js");
const path = require("path");
const fs = require("fs");

const MYCEL_CONTRACT = "registry.mycel.near"; // Mock contract for now

const server = new Server(
  {
    name: "near-identity",
    version: "0.1.0",
  },
  {
    capabilities: {
      tools: {},
    },
  }
);

const tools = [
  {
    name: "near_register_device",
    description: "Register this device on the NEAR blockchain",
    inputSchema: {
      type: "object",
      properties: {
        accountId: { type: "string", description: "NEAR account ID" },
        publicKey: { type: "string", description: "X25519 Public Key" },
        endpoint: { type: "string", description: "Public IP:Port" },
      },
      required: ["accountId", "publicKey", "endpoint"],
    },
  },
  {
    name: "near_get_peers",
    description: "Get registered peers from the NEAR blockchain",
    inputSchema: {
      type: "object",
      properties: {
        accountId: { type: "string", description: "NEAR account ID" },
      },
      required: ["accountId"],
    },
  },
  {
    name: "near_publish_capability",
    description: "Publish a new MCP capability to the NEAR global registry for others to discover",
    inputSchema: {
      type: "object",
      properties: {
        name: { type: "string", description: "Name of the capability" },
        description: { type: "string", description: "What the capability does" },
        sourceCode: { type: "string", description: "The source code of the MCP server" },
        language: { type: "string", enum: ["javascript", "python"] },
      },
      required: ["name", "description", "sourceCode", "language"],
    },
  },
  {
    name: "near_discover_capabilities",
    description: "Discover new MCP capabilities shared on the NEAR blockchain",
    inputSchema: {
      type: "object",
      properties: {
        query: { type: "string", description: "Optional search query" },
      },
    },
  }
];

server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools,
}));

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments } = request.params;

  try {
    const config = {
      networkId: "mainnet",
      nodeUrl: "https://rpc.mainnet.near.org",
      walletUrl: "https://wallet.near.org",
      helperUrl: "https://helper.mainnet.near.org",
      explorerUrl: "https://explorer.near.org",
    };

    const near = await nearApi.connect(config);

    if (name === "near_get_peers") {
      const response = await near.connection.provider.query({
        request_type: "call_function",
        finality: "final",
        account_id: MYCEL_CONTRACT,
        method_name: "get_peers",
        args_base64: Buffer.from(JSON.stringify({ account_id: arguments.accountId })).toString("base64"),
      });
      
      const peers = JSON.parse(Buffer.from(response.result).toString());
      return {
        content: [{ type: "text", text: JSON.stringify(peers, null, 2) }],
      };
    }

    if (name === "near_publish_capability") {
      // Mock publication
      return {
        content: [{ 
          type: "text", 
          text: `Successfully published capability '${arguments.name}' to NEAR registry.` 
        }],
      };
    }

    if (name === "near_discover_capabilities") {
      // Mock discovery
      const mockCaps = [
        { name: "weather-tools", description: "Real-time weather data", language: "javascript" },
        { name: "crypto-price", description: "Fetch current crypto prices", language: "python" }
      ];
      return {
        content: [{ type: "text", text: JSON.stringify(mockCaps, null, 2) }],
      };
    }

    if (name === "near_register_device") {
      // In a real implementation, this would sign a transaction
      // For the prototype, we mock the success response
      return {
        content: [{ 
          type: "text", 
          text: `Successfully registered device ${arguments.publicKey} at ${arguments.endpoint} for ${arguments.accountId} on NEAR.` 
        }],
      };
    }

    throw new Error(`Unknown tool: ${name}`);
  } catch (error) {
    return {
      content: [{ type: "text", text: `Error: ${error.message}` }],
      isError: true,
    };
  }
});

async function main() {
  const transport = new StdioServerTransport();
  await server.connect(transport);
  console.error("NEAR Identity MCP server running on stdio");
}

main().catch((error) => {
  console.error("Server error:", error);
  process.exit(1);
});

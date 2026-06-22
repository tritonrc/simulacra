// graphql.js — POST queries/mutations to /graphql.
// Throws GraphQLError on `errors[]` in the response. Throws plain Error
// on transport / non-OK status.

export class GraphQLError extends Error {
  constructor(messages, raw) {
    super(messages.join('; '));
    this.name = 'GraphQLError';
    this.errors = raw;
  }
}

export async function gql(query, variables) {
  const response = await fetch('/graphql', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ query, variables: variables ?? null }),
  });
  if (!response.ok) {
    throw new Error(`GraphQL HTTP ${response.status}`);
  }
  const payload = await response.json();
  if (payload.errors && payload.errors.length > 0) {
    throw new GraphQLError(payload.errors.map(e => e.message), payload.errors);
  }
  return payload.data;
}

You're tasked with creating an agent memory system on top of GH issues only.

The advantage of using GH issues is clear:
- It offers semantic search: [Improved search for GitHub Issues is now generally available](https://github.com/orgs/community/discussions/190865):

    What's included in this release
    Natural language search across issues: Describe what you're looking for in plain language and GitHub returns conceptually related results, even when the wording doesn't match.
    Issues index and dashboard: Semantic search works both within a single repository and across your repositories on the issues dashboard.
    Hybrid search: When you search with natural language, GitHub combines semantic and keyword matching in the same query, so you get both conceptually related results and exact matches together. Searches using only filters or quotation marks use traditional lexical search for precision.
    Best match sorting: Results are ordered by relevance by default, surfacing the most useful issues first.
    API access: Semantic search is now available through the REST and GraphQL APIs, so you can integrate it into your own tools and workflows.
    API details
    Use the existing /search/issues endpoint with search_type=semantic or search_type=hybrid. The response tells you which search was performed and, if a fallback to lexical occurred, why. If you do not specify a search type, a lexical search will be performed by default.

    You can scope your queries with org:, user:, and repo: qualifiers. Semantic and hybrid queries are rate limited to 10 requests per minute. Standard lexical searches retain existing rate limits.

    If you are accessing search via GraphQL, you can use the searchType argument on the search query with SEMANTIC or HYBRID.
- Generous API rate limits: unless you go crazy, you'll never get rate-limited
- Fast enough: again, unless you wanna go very scalable, latency is bearable.
- Free & infinite db: GH issues is free forever, for everyone.
- Mobility & shareability: because GH issues are attached to a repository, it can easily be transferred to elsewhere or read/contributed by anyone else in your team.

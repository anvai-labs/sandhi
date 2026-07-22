// Type entry point for @anvai-labs/sandhi. Re-exports the napi-generated types and augments
// `ByteStream` with the `Symbol.asyncIterator` that `sandhi.js` installs at runtime (ADR-0047 D10
// step 3c), so `for await (const chunk of stream)` type-checks.

export * from "./index";
export * from "./contracts";
declare module "./index" {
  interface TypedEventStream {
    [Symbol.asyncIterator](): AsyncIterator<string>;
  }
}

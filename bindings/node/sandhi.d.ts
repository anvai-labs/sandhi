// Type entry point for @anvai-labs/sandhi. Re-exports the napi-generated types and augments
// `ByteStream` with the `Symbol.asyncIterator` that `sandhi.js` installs at runtime (ADR-0047 D10
// step 3c), so `for await (const chunk of stream)` type-checks.

export * from "./index";
export * from "./contracts";
import type { ProviderErrorV1 } from "./contracts";

/** Provider-boundary error; `payload` is the parsed ProviderErrorV1 (TD-0008 P2/C). */
export declare class SandhiProviderError extends Error {
  payload: ProviderErrorV1;
  code: string;
  httpStatus: number | null;
  retryable: boolean;
  requestId: string | null;
  details: Record<string, unknown>;
}
declare module "./index" {
  interface TypedEventStream {
    [Symbol.asyncIterator](): AsyncIterator<string>;
  }
}

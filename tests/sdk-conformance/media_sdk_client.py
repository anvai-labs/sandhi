"""One real SDK inline-image call driven by the Rust gateway acceptance test."""

import base64
import hashlib
import importlib.metadata
import json
import sys

ROOT, DIALECT, STREAMING = sys.argv[1], sys.argv[2], sys.argv[3] == "true"
CODEX = sys.argv[4] == "codex"
INSTRUCTIONS = "Describe the supplied synthetic image accurately."
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jYXkAAAAASUVORK5CYII="
)
DATA = "data:image/png;base64," + base64.b64encode(PNG).decode()
TEXT = "Describe this single synthetic pixel."
MODEL = "gpt-4o-mini"
if DIALECT in ("chat_completions", "responses"):
    import openai

    with openai.OpenAI(
        base_url=ROOT + "/v1", api_key="vk_demo", max_retries=0, timeout=5
    ) as client:
        if DIALECT == "chat_completions":
            from openai.types.chat import (
                ChatCompletionContentPartImageParam,
                ChatCompletionContentPartTextParam,
            )

            content = [
                ChatCompletionContentPartImageParam(
                    type="image_url", image_url={"url": DATA, "detail": "auto"}
                ),
                ChatCompletionContentPartTextParam(type="text", text=TEXT),
            ]
            response = client.chat.completions.create(
                model=MODEL,
                max_tokens=64,
                messages=([{"role": "system", "content": INSTRUCTIONS}] if CODEX else [])
                + [{"role": "user", "content": content}],
                stream=STREAMING,
            )
            text = (
                "".join(e.choices[0].delta.content or "" for e in response if e.choices)
                if STREAMING
                else response.choices[0].message.content
            )
        else:
            from openai.types.responses import (
                ResponseInputImageParam,
                ResponseInputTextParam,
            )

            content = [
                ResponseInputImageParam(
                    type="input_image", image_url=DATA, detail="auto"
                ),
                ResponseInputTextParam(type="input_text", text=TEXT),
            ]
            response = client.responses.create(
                model=MODEL,
                max_output_tokens=64,
                input=[{"role": "user", "content": content}],
                **({"instructions": INSTRUCTIONS} if CODEX else {}),
                stream=STREAMING,
            )
            text = (
                "".join(
                    e.delta for e in response if e.type == "response.output_text.delta"
                )
                if STREAMING
                else response.output_text
            )
    sdk = "openai"
elif DIALECT == "anthropic_messages":
    import anthropic
    from anthropic.types import ImageBlockParam, TextBlockParam

    content = [
        ImageBlockParam(
            type="image",
            source={
                "type": "base64",
                "media_type": "image/png",
                "data": base64.b64encode(PNG).decode(),
            },
        ),
        TextBlockParam(type="text", text=TEXT),
    ]
    with anthropic.Anthropic(
        base_url=ROOT, api_key="vk_demo", max_retries=0, timeout=5
    ) as client:
        response = client.messages.create(
            model=MODEL,
            max_tokens=64,
            **({"system": INSTRUCTIONS} if CODEX else {}),
            messages=[{"role": "user", "content": content}],
            stream=STREAMING,
        )
        text = (
            "".join(
                e.delta.text
                for e in response
                if e.type == "content_block_delta" and e.delta.type == "text_delta"
            )
            if STREAMING
            else "".join(b.text for b in response.content if b.type == "text")
        )
    sdk = "anthropic"
else:
    from google import genai
    from google.genai import types

    content = types.Content(
        role="user",
        parts=[
            types.Part.from_bytes(data=PNG, mime_type="image/png"),
            types.Part.from_text(text=TEXT),
        ],
    )
    with genai.Client(
        api_key="vk_demo",
        http_options=types.HttpOptions(
            base_url=ROOT,
            timeout=5000,
            retry_options=types.HttpRetryOptions(attempts=1),
        ),
    ) as client:
        config = types.GenerateContentConfig(
            max_output_tokens=64, system_instruction=INSTRUCTIONS if CODEX else None
        )
        response = (
            client.models.generate_content_stream(
                model=MODEL, contents=content, config=config
            )
            if STREAMING
            else client.models.generate_content(
                model=MODEL, contents=content, config=config
            )
        )
        text = "".join(e.text or "" for e in response) if STREAMING else response.text
    sdk = "google-genai"
assert text == "reply", text
print(
    json.dumps(
        {
            "text": text,
            "dialect": DIALECT,
            "streaming": STREAMING,
            "image_sha256": hashlib.sha256(PNG).hexdigest(),
            "sdk": sdk,
            "sdk_version": importlib.metadata.version(sdk),
        }
    )
)

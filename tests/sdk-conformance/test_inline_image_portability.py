"""Actual Gemini SDK inline bytes must survive native and cross-family requests."""

import base64
import json

import pytest
from google import genai
from google.genai import types

from test_backend_registry import backend_binaries  # noqa: F401
from test_tracked_protocols import FAMILIES, evidence, qualified_origin, tracked_gateway  # noqa: F401

PNG = base64.b64decode(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jYXkAAAAASUVORK5CYII='
)


@pytest.mark.parametrize('family', FAMILIES)
@pytest.mark.parametrize('streaming', [False, True])
def test_actual_gemini_sdk_inline_image_preserves_bytes_and_original_receipt(
    tmp_path, backend_binaries, qualified_origin, family, streaming,
):
    origin, calls, _control = qualified_origin
    with tracked_gateway(tmp_path, backend_binaries, origin, family, streaming=streaming) as (instance, token):
        with genai.Client(api_key=token, http_options=types.HttpOptions(base_url=instance.base)) as client:
            method = client.models.generate_content_stream if streaming else client.models.generate_content
            result = method(
                model='agent',
                contents=[types.Part.from_bytes(data=PNG, mime_type='image/png'),
                          types.Part.from_text(text='Describe this single synthetic pixel.')],
                config=types.GenerateContentConfig(max_output_tokens=64),
            )
            text = ''.join(part.text or '' for part in result) if streaming else result.text
            assert text == 'reply'
        assert len(calls) == 1
        body = calls[0][1]
        encoded = base64.b64encode(PNG).decode()
        if family == 'gemini_generate_content':
            inline = next(part['inlineData'] for item in body['contents'] for part in item['parts'] if 'inlineData' in part)
            assert inline.get('mimeType', inline.get('mime_type')) == 'image/png'
            assert base64.urlsafe_b64decode(inline['data']) == PNG
        elif family == 'anthropic_messages':
            image = next(part['source'] for item in body['messages'] for part in item['content'] if part.get('type') == 'image')
            assert image['type'] == 'base64' and image['media_type'] == 'image/png'
            assert base64.b64decode(image['data']) == PNG
        else:
            assert 'data:image/png;base64,' + encoded in json.dumps(body)
        _request, context, usage, _charge = evidence(instance.database)
        assert context['ingress'] == 'gemini_generate_content'
        assert context['endpoint_family'] == family
        assert usage['completeness'] == 'final'

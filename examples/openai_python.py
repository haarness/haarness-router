"""Use the official OpenAI Python SDK against Haarness Router."""

import os

from openai import OpenAI


client = OpenAI(
    api_key=os.environ["HAARNESS_ROUTER_KEY"],
    base_url=os.getenv("HAARNESS_ROUTER_URL", "http://127.0.0.1:8080/v1"),
)

stream = client.chat.completions.create(
    model=os.getenv("HAARNESS_MODEL", "fast"),
    messages=[{"role": "user", "content": "Explain bounded backpressure briefly."}],
    stream=True,
    stream_options={"include_usage": True},
)

for chunk in stream:
    if chunk.choices and chunk.choices[0].delta.content:
        print(chunk.choices[0].delta.content, end="", flush=True)
    if chunk.usage:
        print(f"\nusage={chunk.usage}")

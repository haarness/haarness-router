// Use the official OpenAI JavaScript SDK against Haarness Router.
import OpenAI from "openai";

const client = new OpenAI({
  apiKey: process.env.HAARNESS_ROUTER_KEY,
  baseURL: process.env.HAARNESS_ROUTER_URL ?? "http://127.0.0.1:8080/v1",
});

const completion = await client.chat.completions.create({
  model: process.env.HAARNESS_MODEL ?? "fast",
  messages: [{ role: "user", content: "Return a short greeting." }],
});

console.log(completion.choices[0].message);

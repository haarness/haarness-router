# frozen_string_literal: true

require "yaml"

secrets_path = ARGV.shift
router_binary = ARGV.shift
router_config = ARGV.shift
abort("usage: run-stackforward.rb SECRETS BINARY CONFIG") unless router_config
router_arguments = ARGV

document = YAML.safe_load(
  File.read(secrets_path),
  permitted_classes: [],
  permitted_symbols: [],
  aliases: false
)
abort("Stackforward secret YAML must contain a mapping") unless document.is_a?(Hash)

def required_secret(document, name)
  value = document[name]
  abort("Stackforward secret YAML is missing #{name}") unless value.is_a?(String) && !value.empty?
  value
end

provider_environment = {
  "OPENAI_API_KEY" => required_secret(document, "openai_api_key"),
  "ANTHROPIC_API_KEY" => required_secret(document, "claude_api_key"),
  "GEMINI_API_KEY" => required_secret(document, "gemini_api_key")
}

has_config_argument = router_arguments.any? do |argument|
  argument == "--config" || argument.start_with?("--config=")
end
command = [router_binary]
command.concat(["--config", router_config]) unless has_config_argument
command.concat(router_arguments)
exec(provider_environment, *command)

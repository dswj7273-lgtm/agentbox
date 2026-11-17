class Agentbox < Formula
  desc "Local flight recorder, safety guard, and step-by-step undo for AI coding agents"
  homepage "https://github.com/dswj7273-lgtm/agentbox"
  url "https://github.com/dswj7273-lgtm/agentbox/archive/refs/tags/v0.1.0.tar.gz"
  license "Apache-2.0"
  head "https://github.com/dswj7273-lgtm/agentbox.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "AgentBox", shell_output("#{bin}/abox --help")
  end
end

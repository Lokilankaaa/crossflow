#include <openssl/curve25519.h>
#include <openssl/hmac.h>
#include <openssl/mem.h>
#include <openssl/sha2.h>

#include <array>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>

// Functional integration spike only. No real identity, user code or network is
// used. This test does not establish an audit or a production pairing protocol.
using Bytes = std::vector<uint8_t>;
using Context = std::unique_ptr<SPAKE2_CTX, decltype(&SPAKE2_CTX_free)>;

void require(bool condition, const char* message) {
  if (!condition) throw std::runtime_error(message);
}

struct Side {
  Context context;
  Bytes message;
  Bytes key;
  Side(spake2_role_t role, const std::string& own, const std::string& peer,
       const std::string& password)
      : context(SPAKE2_CTX_new(role,
            reinterpret_cast<const uint8_t*>(own.data()), own.size(),
            reinterpret_cast<const uint8_t*>(peer.data()), peer.size()),
            SPAKE2_CTX_free), message(SPAKE2_MAX_MSG_SIZE), key(SPAKE2_MAX_KEY_SIZE) {
    require(context != nullptr, "context allocation failed");
    size_t length = 0;
    require(SPAKE2_generate_msg(context.get(), message.data(), &length,
                message.size(), reinterpret_cast<const uint8_t*>(password.data()),
                password.size()) == 1, "generate failed");
    require(length > 0 && length <= message.size(), "invalid message length");
    message.resize(length);
  }
  bool finish(const Bytes& peer) {
    size_t length = 0;
    const bool ok = SPAKE2_process_msg(context.get(), key.data(), &length,
                       key.size(), peer.data(), peer.size()) == 1;
    context.reset();
    if (!ok) { key.clear(); return false; }
    require(length > 0 && length <= key.size(), "invalid key length");
    key.resize(length);
    return true;
  }
  ~Side() { if (!key.empty()) OPENSSL_cleanse(key.data(), key.size()); }
};

void append(Bytes& output, const std::string& field) {
  require(field.size() < 65536, "field too large");
  output.push_back(static_cast<uint8_t>(field.size() >> 8));
  output.push_back(static_cast<uint8_t>(field.size()));
  output.insert(output.end(), field.begin(), field.end());
}

Bytes confirmation(const Bytes& key, const std::string& direction,
                   const std::string& initiator_id, const std::string& responder_id) {
  Bytes input;
  append(input, "Crossflow/PAKE/build-spike/confirmation/v1");
  append(input, direction);
  append(input, initiator_id);
  append(input, responder_id);
  Bytes tag(SHA256_DIGEST_LENGTH);
  unsigned length = 0;
  require(HMAC(EVP_sha256(), key.data(), key.size(), input.data(), input.size(),
               tag.data(), &length) != nullptr && length == tag.size(), "HMAC failed");
  return tag;
}

bool equal(const Bytes& left, const Bytes& right) {
  return left.size() == right.size() &&
         CRYPTO_memcmp(left.data(), right.data(), left.size()) == 0;
}

void success_case() {
  const std::string a = "Crossflow/pair/spike/v1/initiator/noise-hash-and-public-keys-1";
  const std::string b = "Crossflow/pair/spike/v1/responder/noise-hash-and-public-keys-1";
  Side alice(spake2_role_alice, a, b, "01234567");
  Side bob(spake2_role_bob, b, a, "01234567");
  require(alice.finish(bob.message) && bob.finish(alice.message), "same-code exchange failed");
  require(equal(alice.key, bob.key), "same-code keys differ");
  auto ca = confirmation(alice.key, "initiator", a, b);
  auto cb = confirmation(bob.key, "responder", a, b);
  require(equal(ca, confirmation(bob.key, "initiator", a, b)), "initiator confirmation failed");
  require(equal(cb, confirmation(alice.key, "responder", a, b)), "responder confirmation failed");
  require(!equal(ca, cb), "reflected confirmation accepted");
  auto corrupt = ca;
  corrupt[0] ^= 1;
  require(!equal(corrupt, confirmation(bob.key, "initiator", a, b)), "tampered confirmation accepted");
}

void rejected_case(const char* mode) {
  const std::string a = "Crossflow/pair/spike/v1/initiator/binding-1";
  const std::string b = "Crossflow/pair/spike/v1/responder/binding-1";
  const bool wrong_code = std::strcmp(mode, "wrong_code") == 0;
  const bool wrong_role = std::strcmp(mode, "wrong_role") == 0;
  const bool wrong_binding = std::strcmp(mode, "wrong_binding") == 0;
  const std::string bob_a = wrong_binding ? a + "-changed" : a;
  Side alice(spake2_role_alice, a, b, "01234567");
  Side bob(wrong_role ? spake2_role_alice : spake2_role_bob,
           b, bob_a, wrong_code ? "76543210" : "01234567");
  const bool a_finished = alice.finish(bob.message);
  const bool b_finished = bob.finish(alice.message);
  if (wrong_code) require(a_finished && b_finished, "wrong code unexpectedly failed before confirmation");
  if (a_finished && b_finished) {
    require(!equal(alice.key, bob.key), "negative case produced matching keys");
    require(!equal(confirmation(alice.key, "initiator", a, b),
                   confirmation(bob.key, "initiator", bob_a, b)), "negative confirmation accepted");
    require(!equal(confirmation(bob.key, "responder", bob_a, b),
                   confirmation(alice.key, "responder", a, b)), "negative reciprocal confirmation accepted");
  }
}

int main() {
  try {
    for (int repeat = 0; repeat < 32; ++repeat) {
      success_case();
      rejected_case("wrong_code");
      rejected_case("wrong_role");
      rejected_case("wrong_binding");
    }
    std::puts("PASS same_code: equal keys and both confirmation MACs accepted (32 exchanges)");
    std::puts("PASS wrong_code: different keys and both confirmation MACs rejected (32 exchanges)");
    std::puts("PASS wrong_role: no authenticated agreement (32 exchanges)");
    std::puts("PASS changed_binding: no authenticated agreement (32 exchanges)");
    std::puts("PASS reflected_confirmation and tampered_confirmation rejected (32 exchanges)");
    std::printf("SPAKE2_MAX_MSG_SIZE=%d SPAKE2_MAX_KEY_SIZE=%d\n", SPAKE2_MAX_MSG_SIZE, SPAKE2_MAX_KEY_SIZE);
    std::puts("FUNCTIONAL BUILD SPIKE ONLY; NOT A SECURITY AUDIT");
    return 0;
  } catch (const std::exception& error) {
    std::fprintf(stderr, "FAIL %s\n", error.what());
    return 1;
  }
}

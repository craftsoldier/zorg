String swapDepositQrPayload(String address, String? memo) {
  final normalizedAddress = address.trim();
  final normalizedMemo = memo?.trim();
  if (normalizedMemo == null || normalizedMemo.isEmpty) {
    return normalizedAddress;
  }
  final separator = normalizedAddress.contains('?') ? '&' : '?';
  final encodedMemo = Uri.encodeQueryComponent(normalizedMemo);
  return '$normalizedAddress${separator}memo=$encodedMemo';
}

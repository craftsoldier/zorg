String compactSwapAddress(
  String value, {
  int maxLength = 18,
  int prefixLength = 9,
  int suffixLength = 7,
  String separator = ' ... ',
}) {
  final trimmed = value.trim();
  if (trimmed.length <= maxLength) return trimmed;
  return '${trimmed.substring(0, prefixLength)}$separator${trimmed.substring(trimmed.length - suffixLength)}';
}

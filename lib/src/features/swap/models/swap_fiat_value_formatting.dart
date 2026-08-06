String swapFormatFiatValue(double value) {
  if (value <= 0) return '0';
  final digits = value >= 100
      ? 0
      : value >= 1
      ? 2
      : 4;
  return swapTrimFixed(
    _truncateToFractionDigits(value, digits),
    fractionDigits: digits,
  );
}

String swapFormatCompactFiatValue(double value) {
  if (!value.isFinite || value <= 0) return r'$0.00';
  if (value >= 1000000) {
    return '\$${swapTrimFixed(value / 1000000, fractionDigits: 3)}M';
  }
  if (value >= 1000) {
    return '\$${swapTrimFixed(value / 1000, fractionDigits: 2)}K';
  }
  return '\$${value.toStringAsFixed(2)}';
}

String swapTrimFixed(double value, {required int fractionDigits}) {
  var text = value.toStringAsFixed(fractionDigits);
  while (text.contains('.') && text.endsWith('0')) {
    text = text.substring(0, text.length - 1);
  }
  if (text.endsWith('.')) text = text.substring(0, text.length - 1);
  return text;
}

double _truncateToFractionDigits(double value, int fractionDigits) {
  if (!value.isFinite || value <= 0) return 0;
  var factor = 1.0;
  for (var i = 0; i < fractionDigits; i++) {
    factor *= 10;
  }
  return (value * factor).truncateToDouble() / factor;
}

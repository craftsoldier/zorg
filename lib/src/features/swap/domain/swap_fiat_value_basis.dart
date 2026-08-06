class SwapFiatValueBasis {
  const SwapFiatValueBasis({
    required this.capturedAt,
    this.sellUsdUnitPrice,
    this.receiveUsdUnitPrice,
  });

  final double? sellUsdUnitPrice;
  final double? receiveUsdUnitPrice;
  final DateTime capturedAt;

  bool get isUsable =>
      _isUsableUnitPrice(sellUsdUnitPrice) ||
      _isUsableUnitPrice(receiveUsdUnitPrice);

  double? sellUsdValue(double amount) {
    return _usdValue(amount, sellUsdUnitPrice);
  }

  double? receiveUsdValue(double amount) {
    return _usdValue(amount, receiveUsdUnitPrice);
  }
}

bool _isUsableUnitPrice(double? value) {
  return value != null && value.isFinite && value > 0;
}

double? _usdValue(double amount, double? unitPrice) {
  if (!amount.isFinite || amount <= 0 || !_isUsableUnitPrice(unitPrice)) {
    return null;
  }
  return amount * unitPrice!;
}

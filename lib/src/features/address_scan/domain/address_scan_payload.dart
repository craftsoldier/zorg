import '../../../core/zcash/zip321_payment_request.dart';

String? normalizeAddressScanPayload(String? input) {
  final trimmed = input?.trim();
  if (trimmed == null || trimmed.isEmpty) return null;

  final uri = Uri.tryParse(trimmed);
  if (uri == null || uri.scheme.isEmpty) return trimmed;

  final scheme = uri.scheme.toLowerCase();
  return switch (scheme) {
    'zcash' => _zcashAddressFromUri(trimmed, uri) ?? trimmed,
    'ethereum' || 'eth' => _ethereumAddressFromUri(uri) ?? trimmed,
    'near' ||
    'bitcoin' ||
    'litecoin' ||
    'dogecoin' ||
    'solana' ||
    'tron' => _genericAddressFromUri(uri) ?? trimmed,
    _ => trimmed,
  };
}

String? _zcashAddressFromUri(String raw, Uri uri) {
  try {
    return Zip321PaymentRequest.parse(raw).primaryPayment.address.trim();
  } on Zip321ParseException {
    return _genericAddressFromUri(uri);
  }
}

String? _ethereumAddressFromUri(Uri uri) {
  final queryAddress = uri.queryParameters['address']?.trim();
  if (queryAddress != null && queryAddress.isNotEmpty) return queryAddress;

  var path = Uri.decodeComponent(uri.path).trim();
  if (path.startsWith('pay-')) {
    path = path.substring(4);
  }
  if (path.isEmpty) return _genericAddressFromUri(uri);

  final stop = _firstPositiveIndex([
    path.indexOf('@'),
    path.indexOf('/'),
    path.indexOf('?'),
  ]);
  final address = stop == null ? path : path.substring(0, stop);
  return address.trim().isEmpty ? null : address.trim();
}

String? _genericAddressFromUri(Uri uri) {
  final queryAddress = uri.queryParameters['address']?.trim();
  if (queryAddress != null && queryAddress.isNotEmpty) return queryAddress;

  final path = Uri.decodeComponent(uri.path).trim();
  if (path.isNotEmpty) return path;

  final host = Uri.decodeComponent(uri.host).trim();
  if (host.isNotEmpty) return host;
  return null;
}

int? _firstPositiveIndex(Iterable<int> values) {
  final positives = values.where((value) => value > 0).toList()..sort();
  return positives.isEmpty ? null : positives.first;
}

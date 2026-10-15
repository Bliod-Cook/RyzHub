import 'package:flutter/material.dart';

/// Accent colors a member can pick, by name.
const accents = <String, Color>{
  'orange': Color(0xfff0692a),
  'blue': Color(0xff2f6fed),
  'green': Color(0xff10a37f),
  'purple': Color(0xff8b5cf6),
  'pink': Color(0xffe0457b),
};

Color accentColor(String name) => accents[name] ?? accents['orange']!;

@immutable
class HubColors extends ThemeExtension<HubColors> {
  final Color background, sidebar, border, hover, selected, text, muted, subtle;
  final Color bubble, composer, menu, code, accent, onAccent, danger, success;
  final Color warning;

  /// How much of the accent tints the user's messages.
  final double userTint;
  const HubColors({
    required this.background,
    required this.sidebar,
    required this.border,
    required this.hover,
    required this.selected,
    required this.text,
    required this.muted,
    required this.subtle,
    required this.bubble,
    required this.composer,
    required this.menu,
    required this.code,
    required this.accent,
    required this.onAccent,
    required this.danger,
    required this.success,
    required this.warning,
    required this.userTint,
  });

  static const light = HubColors(
    background: Color(0xffffffff),
    sidebar: Color(0xfff9f9f9),
    border: Color(0xffe5e5e5),
    hover: Color(0xffefefef),
    selected: Color(0xffe8e8e8),
    text: Color(0xff0d0d0d),
    muted: Color(0xff5d5d5d),
    subtle: Color(0xff8f8f8f),
    bubble: Color(0xfff4f4f4),
    composer: Color(0xffffffff),
    menu: Color(0xffffffff),
    code: Color(0xfff7f7f7),
    accent: Color(0xfff0692a),
    onAccent: Color(0xffffffff),
    danger: Color(0xffd93025),
    success: Color(0xff10a37f),
    warning: Color(0xffe8a317),
    userTint: .12,
  );

  static const dark = HubColors(
    background: Color(0xff212121),
    sidebar: Color(0xff181818),
    border: Color(0x1affffff),
    hover: Color(0xff2a2a2a),
    selected: Color(0xff303030),
    text: Color(0xffececec),
    muted: Color(0xffb4b4b4),
    subtle: Color(0xff8f8f8f),
    bubble: Color(0xff303030),
    composer: Color(0xff303030),
    menu: Color(0xff353535),
    code: Color(0xff171717),
    accent: Color(0xfff0692a),
    onAccent: Color(0xffffffff),
    danger: Color(0xffff6b5e),
    success: Color(0xff3ecf9e),
    warning: Color(0xfff2c14e),
    userTint: .24,
  );

  /// The user's messages, tinted with the accent.
  Color get userBubble =>
      Color.alphaBlend(accent.withValues(alpha: userTint), background);

  static HubColors of(BuildContext context) =>
      Theme.of(context).extension<HubColors>()!;

  @override
  HubColors copyWith({Color? accent}) => HubColors(
    background: background,
    sidebar: sidebar,
    border: border,
    hover: hover,
    selected: selected,
    text: text,
    muted: muted,
    subtle: subtle,
    bubble: bubble,
    composer: composer,
    menu: menu,
    code: code,
    accent: accent ?? this.accent,
    onAccent: onAccent,
    danger: danger,
    success: success,
    warning: warning,
    userTint: userTint,
  );

  @override
  HubColors lerp(covariant HubColors? other, double t) {
    if (other == null) return this;
    Color l(Color a, Color b) => Color.lerp(a, b, t)!;
    return HubColors(
      background: l(background, other.background),
      sidebar: l(sidebar, other.sidebar),
      border: l(border, other.border),
      hover: l(hover, other.hover),
      selected: l(selected, other.selected),
      text: l(text, other.text),
      muted: l(muted, other.muted),
      subtle: l(subtle, other.subtle),
      bubble: l(bubble, other.bubble),
      composer: l(composer, other.composer),
      menu: l(menu, other.menu),
      code: l(code, other.code),
      accent: l(accent, other.accent),
      onAccent: l(onAccent, other.onAccent),
      danger: l(danger, other.danger),
      success: l(success, other.success),
      warning: l(warning, other.warning),
      userTint: userTint + (other.userTint - userTint) * t,
    );
  }
}

/// Spacing with a role; other gaps are set where they are used.
abstract final class Space {
  /// The usual space between neighbours, and around them in a row.
  static const double gap = 8;

  /// Text from the edge of the box it sits in: a menu, a field, a bar.
  static const double inset = 14;

  /// Content from the edge of the page, a panel or a card.
  static const double edge = 16;
}

/// Corner radii, by the size of what they round.
abstract final class Radii {
  /// Thumbnails, tooltips and small buttons.
  static const double small = 8;

  /// Rows, icon buttons and code boxes.
  static const double control = 10;

  /// Fields, menu items, bars and chips.
  static const double field = 12;

  /// Blocks of the timeline: code, and items that are not messages.
  static const double block = 14;

  /// Menus, cards and panels.
  static const double card = 16;
  static const double dialog = 20;
}

/// Type sizes.
abstract final class FontSizes {
  /// Captions, hints and the detail under a line.
  static const double caption = 12;

  /// Labels, buttons and pills.
  static const double label = 13;

  /// A run's steps, between labels and body text.
  static const double step = 13.5;
  static const double body = 14;

  /// Messages and the composer.
  static const double message = 15;
  static const double title = 16;
}

const fontFamily = 'RyzHub Sans';
const fontFallback = ['Microsoft YaHei', 'Noto Sans CJK SC', 'PingFang SC'];

/// Code, in the bundled even-width font; CJK falls back to the UI font.
/// Masked fields use it too: the UI font's bullet is a full-width CJK dot.
const codeStyle = TextStyle(
  fontFamily: '0xProto',
  fontFamilyFallback: [fontFamily, ...fontFallback],
);

/// Model identifiers and reasoning levels: even glyph widths keep the label
/// steady as values change.
TextStyle modelLabelStyle({required double size, required Color color}) =>
    codeStyle.copyWith(
      fontSize: size,
      color: color,
      height: 1.2,
      leadingDistribution: TextLeadingDistribution.even,
    );

/// Theme-level styles replace, rather than merge with, the text theme, so each
/// one names the bundled CJK font explicitly.
TextStyle _font({double? fontSize, Color? color}) => TextStyle(
  fontFamily: fontFamily,
  fontFamilyFallback: fontFallback,
  fontSize: fontSize,
  color: color,
  leadingDistribution: TextLeadingDistribution.even,
);

/// Extra line height goes equally above and below the glyphs. The default
/// splits it by the font's ascent:descent ratio, which pushes text down.
TextTheme _evenLeading(TextTheme t) {
  TextStyle? even(TextStyle? s) =>
      s?.copyWith(leadingDistribution: TextLeadingDistribution.even);
  return TextTheme(
    displayLarge: even(t.displayLarge),
    displayMedium: even(t.displayMedium),
    displaySmall: even(t.displaySmall),
    headlineLarge: even(t.headlineLarge),
    headlineMedium: even(t.headlineMedium),
    headlineSmall: even(t.headlineSmall),
    titleLarge: even(t.titleLarge),
    titleMedium: even(t.titleMedium),
    titleSmall: even(t.titleSmall),
    bodyLarge: even(t.bodyLarge),
    bodyMedium: even(t.bodyMedium),
    bodySmall: even(t.bodySmall),
    labelLarge: even(t.labelLarge),
    labelMedium: even(t.labelMedium),
    labelSmall: even(t.labelSmall),
  );
}

ThemeData buildTheme(Brightness brightness, Color accent) {
  final c = (brightness == Brightness.dark ? HubColors.dark : HubColors.light)
      .copyWith(accent: accent);
  final scheme = ColorScheme.fromSeed(seedColor: accent, brightness: brightness)
      .copyWith(
        primary: accent,
        onPrimary: c.onAccent,
        secondary: accent,
        surface: c.background,
        onSurface: c.text,
        onSurfaceVariant: c.muted,
        outline: c.border,
        outlineVariant: c.border,
        surfaceContainerLowest: c.background,
        surfaceContainerLow: c.sidebar,
        surfaceContainer: c.menu,
        surfaceContainerHigh: c.menu,
        surfaceContainerHighest: c.bubble,
        error: c.danger,
      );
  final radius = BorderRadius.circular(Radii.field);
  final theme = ThemeData(
    useMaterial3: true,
    brightness: brightness,
    colorScheme: scheme,
    fontFamily: fontFamily,
    fontFamilyFallback: fontFallback,
    scaffoldBackgroundColor: c.background,
    canvasColor: c.background,
    extensions: [c],
    splashFactory: InkSparkle.constantTurbulenceSeedSplashFactory,
    hoverColor: c.hover,
    dividerTheme: DividerThemeData(color: c.border, thickness: 1, space: 1),
    iconTheme: IconThemeData(color: c.muted, size: 20),
    textSelectionTheme: TextSelectionThemeData(
      cursorColor: accent,
      selectionColor: accent.withValues(alpha: .25),
    ),
    inputDecorationTheme: InputDecorationTheme(
      isDense: true,
      contentPadding: const EdgeInsets.symmetric(
        horizontal: Space.inset,
        vertical: Space.inset,
      ),
      border: OutlineInputBorder(
        borderRadius: radius,
        borderSide: BorderSide(color: c.border),
      ),
      enabledBorder: OutlineInputBorder(
        borderRadius: radius,
        borderSide: BorderSide(color: c.border),
      ),
      focusedBorder: OutlineInputBorder(
        borderRadius: radius,
        borderSide: BorderSide(color: c.muted),
      ),
      labelStyle: _font(color: c.muted),
      hintStyle: _font(color: c.subtle),
    ),
    popupMenuTheme: PopupMenuThemeData(
      color: c.menu,
      surfaceTintColor: Colors.transparent,
      elevation: 6,
      shadowColor: Colors.black.withValues(alpha: .18),
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(Radii.card),
        side: BorderSide(color: c.border),
      ),
      textStyle: _font(fontSize: FontSizes.body, color: c.text),
      menuPadding: const EdgeInsets.all(Space.gap),
    ),
    menuTheme: MenuThemeData(
      style: MenuStyle(
        backgroundColor: WidgetStatePropertyAll(c.menu),
        surfaceTintColor: const WidgetStatePropertyAll(Colors.transparent),
        shape: WidgetStatePropertyAll(
          RoundedRectangleBorder(
            borderRadius: BorderRadius.circular(Radii.card),
            side: BorderSide(color: c.border),
          ),
        ),
        elevation: const WidgetStatePropertyAll(6),
        shadowColor: WidgetStatePropertyAll(
          Colors.black.withValues(alpha: .18),
        ),
        padding: const WidgetStatePropertyAll(EdgeInsets.all(Space.gap)),
      ),
    ),
    // Items sit inset from the panel edge with a rounded highlight.
    menuButtonTheme: MenuButtonThemeData(
      style: ButtonStyle(
        padding: const WidgetStatePropertyAll(
          EdgeInsets.symmetric(horizontal: Space.inset),
        ),
        minimumSize: const WidgetStatePropertyAll(Size(0, 38)),
        shape: WidgetStatePropertyAll(
          RoundedRectangleBorder(
            borderRadius: BorderRadius.circular(Radii.field),
          ),
        ),
        foregroundColor: WidgetStatePropertyAll(c.text),
        iconColor: WidgetStatePropertyAll(c.muted),
        overlayColor: WidgetStatePropertyAll(c.hover),
        textStyle: WidgetStatePropertyAll(_font(fontSize: FontSizes.body)),
      ),
    ),
    tooltipTheme: TooltipThemeData(
      waitDuration: const Duration(milliseconds: 500),
      decoration: BoxDecoration(
        color: brightness == Brightness.dark ? c.menu : c.text,
        borderRadius: BorderRadius.circular(Radii.small),
      ),
      textStyle: _font(
        fontSize: FontSizes.caption,
        color: brightness == Brightness.dark ? c.text : c.background,
      ),
    ),
    dialogTheme: DialogThemeData(
      backgroundColor: c.menu,
      surfaceTintColor: Colors.transparent,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(Radii.dialog),
      ),
    ),
    drawerTheme: DrawerThemeData(
      backgroundColor: c.sidebar,
      surfaceTintColor: Colors.transparent,
      shape: const RoundedRectangleBorder(),
      width: 280,
    ),
    snackBarTheme: SnackBarThemeData(
      behavior: SnackBarBehavior.floating,
      backgroundColor: brightness == Brightness.dark ? c.menu : c.text,
      contentTextStyle: _font(
        color: brightness == Brightness.dark ? c.text : c.background,
      ),
      shape: RoundedRectangleBorder(borderRadius: radius),
      width: 420,
    ),
    switchTheme: SwitchThemeData(
      thumbColor: const WidgetStatePropertyAll(Colors.white),
      trackColor: WidgetStateProperty.resolveWith(
        (s) => s.contains(WidgetState.selected) ? accent : c.border,
      ),
      trackOutlineColor: const WidgetStatePropertyAll(Colors.transparent),
      thumbIcon: const WidgetStatePropertyAll(null),
    ),
    filledButtonTheme: FilledButtonThemeData(
      style: FilledButton.styleFrom(
        backgroundColor: c.text,
        foregroundColor: c.background,
        shape: const StadiumBorder(),
        padding: const EdgeInsets.symmetric(horizontal: 18, vertical: 12),
      ),
    ),
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: OutlinedButton.styleFrom(
        foregroundColor: c.text,
        side: BorderSide(color: c.border),
        shape: const StadiumBorder(),
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 10),
      ),
    ),
    textButtonTheme: TextButtonThemeData(
      style: TextButton.styleFrom(
        foregroundColor: c.text,
        shape: const StadiumBorder(),
      ),
    ),
    iconButtonTheme: IconButtonThemeData(
      style: IconButton.styleFrom(
        foregroundColor: c.muted,
        // Standard density keeps room between a glyph and its highlight; the
        // compact desktop default hugs it.
        visualDensity: VisualDensity.standard,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(Radii.control),
        ),
      ),
    ),
    chipTheme: ChipThemeData(
      backgroundColor: c.composer,
      selectedColor: accent.withValues(alpha: .14),
      checkmarkColor: accent,
      side: BorderSide(color: c.border),
      shape: const StadiumBorder(),
      labelStyle: _font(fontSize: FontSizes.label, color: c.text),
      padding: const EdgeInsets.symmetric(horizontal: 4),
    ),
    segmentedButtonTheme: SegmentedButtonThemeData(
      style: SegmentedButton.styleFrom(
        backgroundColor: c.bubble,
        foregroundColor: c.subtle,
        selectedBackgroundColor: c.background,
        selectedForegroundColor: c.text,
        side: BorderSide.none,
        shape: const StadiumBorder(),
        visualDensity: VisualDensity.compact,
        textStyle: _font(fontSize: FontSizes.label),
      ),
    ),
    progressIndicatorTheme: ProgressIndicatorThemeData(color: accent),
  );
  return theme.copyWith(
    textTheme: _evenLeading(theme.textTheme),
    primaryTextTheme: _evenLeading(theme.primaryTextTheme),
  );
}

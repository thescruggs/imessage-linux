# SPDX-License-Identifier: SSPL-1.0

# Everything except main() lives in a static library so the tests can drive
# the same client, models and QML as the app.
add_library(kmessages_core STATIC
    types.cpp types.h
    daemonclient.cpp daemonclient.h
    outgoingfiles.cpp outgoingfiles.h
    chatlistmodel.cpp chatlistmodel.h
    messagelistmodel.cpp messagelistmodel.h
    handlelookupmodel.cpp handlelookupmodel.h
    messagesearchmodel.cpp messagesearchmodel.h
    appsettings.cpp appsettings.h
    launcherentry.cpp launcherentry.h
    contactresolver.cpp contactresolver.h
    contactsuggestionmodel.cpp contactsuggestionmodel.h
    recipientpickermodel.cpp recipientpickermodel.h
    emojisuggestionmodel.cpp emojisuggestionmodel.h
    macsync.cpp macsync.h
    appcontroller.cpp appcontroller.h
    qmlsetup.cpp qmlsetup.h
)
target_include_directories(kmessages_core PUBLIC ${CMAKE_CURRENT_SOURCE_DIR})
target_compile_options(kmessages_core PRIVATE -Wall -Wextra)
target_compile_definitions(kmessages_core PUBLIC TRANSLATION_DOMAIN="kmessages")
target_link_libraries(kmessages_core PUBLIC
    Qt6::Core Qt6::Gui Qt6::DBus Qt6::Qml Qt6::Quick Qt6::Widgets
    KF6::I18n KF6::I18nQml KF6::ConfigCore KF6::Notifications KF6::StatusNotifierItem KF6::IdleTime
)
if(KMESSAGES_WITH_KPEOPLE)
    target_compile_definitions(kmessages_core PUBLIC KMESSAGES_WITH_KPEOPLE=1)
    target_link_libraries(kmessages_core PUBLIC KF6::People KF6::PeopleBackend)
else()
    target_compile_definitions(kmessages_core PUBLIC KMESSAGES_WITH_KPEOPLE=0)
endif()

set(KMESSAGES_QML
    qml/Main.qml
    qml/ChatListPage.qml
    qml/ChatListDelegate.qml
    qml/ConversationPage.qml
    qml/MessageDelegate.qml
    qml/AttachmentView.qml
    qml/LinkPreview.qml
    qml/ReactionChips.qml
    qml/TypingBubble.qml
    qml/Composer.qml
    qml/EmojiPicker.qml
    qml/TapbackPicker.qml
    qml/MessageMenu.qml
    qml/NewMessagePage.qml
    qml/LoginDialog.qml
    qml/SettingsPage.qml
    qml/ChatDetailsPage.qml
    qml/StatusFooter.qml
)
qt_add_resources(kmessages_core "kmessages_qml" PREFIX "/kmessages" FILES ${KMESSAGES_QML})
# Emoji keywords from Unicode CLDR (tools/gen-emoji-keywords.py; Unicode
# License v3, see data/emoji-keywords.LICENSE): qrc:/kmessages/data/…
qt_add_resources(kmessages_core "kmessages_data" PREFIX "/kmessages/data" BASE ../data FILES ../data/emoji-keywords.json)

add_executable(kmessages main.cpp)
target_compile_options(kmessages PRIVATE -Wall -Wextra)
target_link_libraries(kmessages PRIVATE
    kmessages_core Qt6::QuickControls2 KF6::CoreAddons KF6::IconThemes KF6::DBusAddons)
install(TARGETS kmessages ${KDE_INSTALL_TARGETS_DEFAULT_ARGS})

# AUTOMOC adds <target>_autogen/include to the include path but only creates it
# when a header needs it; pre-create it so -Wmissing-include-dirs stays quiet.
file(MAKE_DIRECTORY ${CMAKE_CURRENT_BINARY_DIR}/kmessages_core_autogen/include)

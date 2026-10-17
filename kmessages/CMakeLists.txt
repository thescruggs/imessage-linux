# SPDX-License-Identifier: SSPL-1.0
cmake_minimum_required(VERSION 3.25)
project(kmessages VERSION 0.1.10 LANGUAGES CXX)

set(QT_MIN_VERSION 6.10.0)
set(KF_MIN_VERSION 6.8.0)

find_package(ECM ${KF_MIN_VERSION} REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

include(KDEInstallDirs)
include(KDECMakeSettings)
include(KDECompilerSettings NO_POLICY_SCOPE)
include(FeatureSummary)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(Qt6 ${QT_MIN_VERSION} REQUIRED COMPONENTS Core Gui DBus Qml Quick QuickControls2 Widgets)
find_package(KF6 ${KF_MIN_VERSION} REQUIRED COMPONENTS
    Kirigami I18n CoreAddons Config Notifications StatusNotifierItem IconThemes DBusAddons)
find_package(KF6KirigamiAddons 1.0 REQUIRED)
set_package_properties(KF6KirigamiAddons PROPERTIES TYPE RUNTIME
    DESCRIPTION "FormCard, Avatar and delegates used by the QML UI")
option(KMESSAGES_WITH_KPEOPLE "Show contact names and photos from KPeople" ON)
if(KMESSAGES_WITH_KPEOPLE)
    find_package(KF6People ${KF_MIN_VERSION})
    set_package_properties(KF6People PROPERTIES TYPE RECOMMENDED
        DESCRIPTION "Contact names and photos (vCards, Akonadi …), else chats show raw handles")
    if(NOT KF6People_FOUND)
        message(WARNING "KF6People not found: building without contact names (chats show handles)")
        set(KMESSAGES_WITH_KPEOPLE OFF)
    endif()
endif()
add_feature_info(KPeople KMESSAGES_WITH_KPEOPLE "contact names and photos")

find_package(KF6QQC2DesktopStyle ${KF_MIN_VERSION})
set_package_properties(KF6QQC2DesktopStyle PROPERTIES TYPE RUNTIME
    DESCRIPTION "Breeze look for Qt Quick Controls")

add_compile_definitions(KMESSAGES_VERSION="${PROJECT_VERSION}")

add_subdirectory(src)

if(BUILD_TESTING)
    find_package(Qt6 ${QT_MIN_VERSION} REQUIRED COMPONENTS Test)
    add_subdirectory(tests)
endif()

install(FILES org.imessage.kmessages.desktop DESTINATION ${KDE_INSTALL_APPDIR})
install(FILES org.imessage.kmessages.metainfo.xml DESTINATION ${KDE_INSTALL_METAINFODIR})
install(FILES kmessages.notifyrc DESTINATION ${KDE_INSTALL_KNOTIFYRCDIR})
install(FILES src/dbus/org.imessage.Daemon1.xml DESTINATION ${KDE_INSTALL_DBUSINTERFACEDIR})

feature_summary(WHAT ALL INCLUDE_QUIET_PACKAGES FATAL_ON_MISSING_REQUIRED_PACKAGES)
